// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Low-level OTLP metrics export.
//!
//! The initial metrics surface supports cumulative monotonic sums and gauges.
//! Histograms and summaries are recognized by [`MetricShape`] but return
//! [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) until a caller-facing
//! allocation and aggregation model is added.
//! Export runtime behavior is delegated to the shared unary export client; this
//! module only builds OTLP metric messages and applies local size limits.
//!
//! ```
//! use kimojio_stack_opentelemetry::{
//!     ExportLimits, GaugeDataPoint, InstrumentationScope, MetricBatch,
//!     MonotonicSumDataPoint, NumberValue, Resource,
//! };
//!
//! let batch = MetricBatch::new(Resource::new(), InstrumentationScope::new("worker"))
//!     .with_monotonic_sum(MonotonicSumDataPoint::new("requests", 1, 2, NumberValue::I64(10)))
//!     .with_gauge(GaugeDataPoint::new("queue.depth", 2, NumberValue::I64(3)));
//! let encoded = batch.encode_request(ExportLimits::default()).unwrap();
//! assert!(!encoded.is_empty());
//! ```

use std::collections::BTreeMap;

use kimojio_stack::RuntimeContext;
use kimojio_stack_http::{HttpConfig, StackTransport, h2};
use prost::Message;

use crate::{
    Error, ExportClientConfig, ExportLimits, InstrumentationScope, KeyValue, Resource,
    check_encoded_len, client::UnaryExportClient, proto, validate_non_empty,
};

/// OTLP metrics unary export path.
pub const METRICS_SERVICE_PATH: &str =
    "/opentelemetry.proto.collector.metrics.v1.MetricsService/Export";

/// Metric shapes recognized by the low-level validation surface.
///
/// Use [`validate_supported`](Self::validate_supported) when accepting
/// user-selected metric shapes before building a batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricShape {
    /// Cumulative monotonic sum.
    MonotonicSum,
    /// Last-value gauge.
    Gauge,
    /// Recognized but not exported by this crate yet.
    Histogram,
    /// Recognized but not exported by this crate yet.
    Summary,
}

impl MetricShape {
    /// Returns whether this metric shape is supported by the initial crate scope.
    pub fn validate_supported(self) -> Result<(), Error> {
        match self {
            Self::MonotonicSum | Self::Gauge => Ok(()),
            Self::Histogram => Err(Error::Unsupported("histogram metrics")),
            Self::Summary => Err(Error::Unsupported("summary metrics")),
        }
    }
}

/// Supported numeric metric values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NumberValue {
    /// Floating-point value.
    F64(f64),
    /// Signed integer value.
    I64(i64),
}

impl NumberValue {
    fn into_proto(self) -> proto::number_data_point::Value {
        match self {
            Self::F64(value) => proto::number_data_point::Value::AsDouble(value),
            Self::I64(value) => proto::number_data_point::Value::AsInt(value),
        }
    }
}

/// A cumulative monotonic sum data point.
///
/// Multiple points with the same name are grouped into one OTLP metric. The
/// start timestamp must not be later than the point timestamp.
#[derive(Clone, Debug, PartialEq)]
pub struct MonotonicSumDataPoint {
    name: String,
    start_time_unix_nano: u64,
    time_unix_nano: u64,
    value: NumberValue,
    attributes: Vec<KeyValue>,
}

impl MonotonicSumDataPoint {
    /// Creates a monotonic sum data point.
    pub fn new(
        name: impl Into<String>,
        start_time_unix_nano: u64,
        time_unix_nano: u64,
        value: NumberValue,
    ) -> Self {
        Self {
            name: name.into(),
            start_time_unix_nano,
            time_unix_nano,
            value,
            attributes: Vec::new(),
        }
    }

    /// Adds an attribute.
    pub fn with_attribute(mut self, attribute: KeyValue) -> Self {
        self.attributes.push(attribute);
        self
    }
}

/// A gauge data point.
///
/// Multiple points with the same name are grouped into one OTLP metric.
#[derive(Clone, Debug, PartialEq)]
pub struct GaugeDataPoint {
    name: String,
    time_unix_nano: u64,
    value: NumberValue,
    attributes: Vec<KeyValue>,
}

impl GaugeDataPoint {
    /// Creates a gauge data point.
    pub fn new(name: impl Into<String>, time_unix_nano: u64, value: NumberValue) -> Self {
        Self {
            name: name.into(),
            time_unix_nano,
            value,
            attributes: Vec::new(),
        }
    }

    /// Adds an attribute.
    pub fn with_attribute(mut self, attribute: KeyValue) -> Self {
        self.attributes.push(attribute);
        self
    }
}

/// Caller-owned batch of supported metric data points.
///
/// A metric name may appear as either a sum or a gauge, but not both in the same
/// batch. `into_request` validates this before constructing the OTLP request.
#[derive(Clone, Debug, PartialEq)]
pub struct MetricBatch {
    resource: Resource,
    scope: InstrumentationScope,
    sums: Vec<MonotonicSumDataPoint>,
    gauges: Vec<GaugeDataPoint>,
}

impl MetricBatch {
    /// Creates a metric batch.
    pub fn new(resource: Resource, scope: InstrumentationScope) -> Self {
        Self {
            resource,
            scope,
            sums: Vec::new(),
            gauges: Vec::new(),
        }
    }

    /// Adds a monotonic sum data point.
    pub fn with_monotonic_sum(mut self, point: MonotonicSumDataPoint) -> Self {
        self.sums.push(point);
        self
    }

    /// Adds a gauge data point.
    pub fn with_gauge(mut self, point: GaugeDataPoint) -> Self {
        self.gauges.push(point);
        self
    }

    /// Returns true if the batch contains no metric data points.
    pub fn is_empty(&self) -> bool {
        self.sums.is_empty() && self.gauges.is_empty()
    }

    /// Builds an OTLP export request.
    ///
    /// This validates metric names, timestamp intervals, and shape/name
    /// consistency. Use [`encode_request`](Self::encode_request) for tests that
    /// also need local encoded-size enforcement.
    pub fn into_request(self) -> Result<proto::ExportMetricsServiceRequest, Error> {
        let mut sums: BTreeMap<String, Vec<proto::NumberDataPoint>> = BTreeMap::new();
        for point in self.sums {
            validate_non_empty(&point.name, "metric name")?;
            if point.start_time_unix_nano > point.time_unix_nano {
                return Err(Error::Validation("monotonic sum timestamp interval"));
            }
            sums.entry(point.name).or_default().push(number_data_point(
                point.start_time_unix_nano,
                point.time_unix_nano,
                point.value,
                point.attributes,
            ));
        }
        let mut gauges: BTreeMap<String, Vec<proto::NumberDataPoint>> = BTreeMap::new();
        for point in self.gauges {
            validate_non_empty(&point.name, "metric name")?;
            if sums.contains_key(&point.name) {
                return Err(Error::Validation("metric name used with multiple shapes"));
            }
            gauges
                .entry(point.name)
                .or_default()
                .push(number_data_point(
                    0,
                    point.time_unix_nano,
                    point.value,
                    point.attributes,
                ));
        }
        let mut metrics = Vec::with_capacity(sums.len() + gauges.len());
        metrics.extend(sums.into_iter().map(monotonic_sum_to_metric));
        metrics.extend(gauges.into_iter().map(gauge_to_metric));

        Ok(proto::ExportMetricsServiceRequest {
            resource_metrics: vec![proto::ResourceMetrics {
                resource: Some(self.resource.into_proto()),
                scope_metrics: vec![proto::ScopeMetrics {
                    scope: Some(self.scope.into_proto()),
                    metrics,
                }],
            }],
        })
    }

    fn into_checked_request(
        self,
        limits: ExportLimits,
    ) -> Result<proto::ExportMetricsServiceRequest, Error> {
        let request = self.into_request()?;
        check_encoded_len(request.encoded_len(), limits)?;
        Ok(request)
    }

    /// Encodes an OTLP export request after applying size limits.
    pub fn encode_request(self, limits: ExportLimits) -> Result<Vec<u8>, Error> {
        let request = self.into_checked_request(limits)?;
        Ok(request.encode_to_vec())
    }
}

fn monotonic_sum_to_metric(
    (name, data_points): (String, Vec<proto::NumberDataPoint>),
) -> proto::Metric {
    proto::Metric {
        name,
        description: String::new(),
        unit: String::new(),
        data: Some(proto::metric::Data::Sum(proto::Sum {
            data_points,
            aggregation_temporality: proto::AggregationTemporality::Cumulative as i32,
            is_monotonic: true,
        })),
    }
}

fn gauge_to_metric((name, data_points): (String, Vec<proto::NumberDataPoint>)) -> proto::Metric {
    proto::Metric {
        name,
        description: String::new(),
        unit: String::new(),
        data: Some(proto::metric::Data::Gauge(proto::Gauge { data_points })),
    }
}

fn number_data_point(
    start_time_unix_nano: u64,
    time_unix_nano: u64,
    value: NumberValue,
    attributes: Vec<KeyValue>,
) -> proto::NumberDataPoint {
    proto::NumberDataPoint {
        attributes: attributes.into_iter().map(KeyValue::into_proto).collect(),
        start_time_unix_nano,
        time_unix_nano,
        value: Some(value.into_proto()),
    }
}

/// Low-level metrics export result.
///
/// An all-zero/default result means the receiver did not report partial failure.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MetricsExportResult {
    rejected_data_points: i64,
    error_message: String,
}

impl MetricsExportResult {
    /// Converts an OTLP metrics export response into a low-level result.
    pub fn from_response(response: proto::ExportMetricsServiceResponse) -> Self {
        response
            .partial_success
            .map_or_else(Self::default, |partial| Self {
                rejected_data_points: partial.rejected_data_points,
                error_message: partial.error_message,
            })
    }

    /// Decodes an OTLP metrics export response.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let response = proto::ExportMetricsServiceResponse::decode(bytes)
            .map_err(|_| Error::Validation("metrics export response"))?;
        Ok(Self::from_response(response))
    }

    /// Number of rejected data points reported by the receiver.
    pub fn rejected_data_points(&self) -> i64 {
        self.rejected_data_points
    }

    /// Receiver-provided rejection message.
    pub fn error_message(&self) -> &str {
        &self.error_message
    }
}

/// Low-level OTLP metrics export client.
///
/// The client owns one gRPC connection. It performs one unary export per
/// [`export`](Self::export) call and leaves retry/batching policy to the caller.
pub struct MetricsClient {
    inner: UnaryExportClient,
}

impl MetricsClient {
    /// Creates a metrics client from a caller-created HTTP/2 connection.
    pub fn new(http: h2::ClientConnection, config: ExportClientConfig) -> Self {
        Self {
            inner: UnaryExportClient::new(http, config),
        }
    }

    /// Creates a metrics client from a caller-created stack transport.
    pub fn from_transport(
        transport: StackTransport,
        http_config: HttpConfig,
        config: ExportClientConfig,
    ) -> Self {
        Self {
            inner: UnaryExportClient::from_transport(transport, http_config, config),
        }
    }

    /// Creates a plaintext metrics client from a connected socket.
    pub fn plaintext(
        socket: kimojio_stack::OwnedFd,
        http_config: HttpConfig,
        config: ExportClientConfig,
    ) -> Self {
        Self {
            inner: UnaryExportClient::plaintext(socket, http_config, config),
        }
    }

    /// Creates a TLS metrics client from an established stack TLS stream.
    #[cfg(feature = "tls")]
    pub fn tls(
        stream: kimojio_stack_tls::TlsStream,
        http_config: HttpConfig,
        config: ExportClientConfig,
    ) -> Self {
        Self {
            inner: UnaryExportClient::tls(stream, http_config, config),
        }
    }

    /// Exports one caller-owned batch of metric data.
    ///
    /// Empty batches return `Ok(MetricsExportResult::default())` without sending
    /// a gRPC request.
    pub fn export(
        &mut self,
        cx: &RuntimeContext<'_>,
        batch: MetricBatch,
    ) -> Result<MetricsExportResult, Error> {
        if batch.is_empty() {
            return Ok(MetricsExportResult::default());
        }
        let request = batch.into_checked_request(self.inner.export_limits())?;
        let response = self
            .inner
            .export::<_, proto::ExportMetricsServiceResponse>(cx, METRICS_SERVICE_PATH, &request)?;
        Ok(MetricsExportResult::from_response(response))
    }

    /// Finishes the client by closing the underlying gRPC connection.
    pub fn finish(self, cx: &RuntimeContext<'_>) -> Result<(), Error> {
        self.inner.finish(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::hint::black_box;

    use kimojio_stack::{Runtime, once};
    use kimojio_stack_grpc::{ServerConfig, Status, StatusCode, UnaryReply, UnaryServer};
    use kimojio_stack_http::{HttpConfig, StackTransport, h2};
    use rustix::net::{AddressFamily, RecvFlags, SocketFlags, SocketType, recv, socketpair};

    use super::*;
    use crate::allocation_tracking;
    use crate::{AnyValue, ErrorKind};

    fn batch() -> MetricBatch {
        MetricBatch::new(
            Resource::new().with_attribute(KeyValue::new(
                "service.name",
                AnyValue::String("test-service".into()),
            )),
            InstrumentationScope::new("test-scope"),
        )
        .with_monotonic_sum(
            MonotonicSumDataPoint::new("requests", 1, 10, NumberValue::I64(7))
                .with_attribute(KeyValue::new("route", AnyValue::String("/".into()))),
        )
        .with_gauge(GaugeDataPoint::new(
            "temperature",
            10,
            NumberValue::F64(42.5),
        ))
    }

    #[test]
    fn metric_batch_encodes_sum_and_gauge() {
        let request = batch().into_request().unwrap();
        let metrics = &request.resource_metrics[0].scope_metrics[0].metrics;

        assert_eq!(metrics.len(), 2);
        assert!(matches!(metrics[0].data, Some(proto::metric::Data::Sum(_))));
        assert!(matches!(
            metrics[1].data,
            Some(proto::metric::Data::Gauge(_))
        ));
    }

    #[test]
    fn empty_metric_batch_is_deterministic_noop_request() {
        let request = MetricBatch::new(Resource::new(), InstrumentationScope::new("scope"))
            .into_request()
            .unwrap();

        assert_eq!(request.resource_metrics.len(), 1);
        assert!(
            request.resource_metrics[0].scope_metrics[0]
                .metrics
                .is_empty()
        );
    }

    #[test]
    fn duplicate_metric_names_group_into_one_metric_per_shape() {
        let request = MetricBatch::new(Resource::new(), InstrumentationScope::new("scope"))
            .with_monotonic_sum(MonotonicSumDataPoint::new(
                "requests",
                1,
                10,
                NumberValue::I64(7),
            ))
            .with_monotonic_sum(MonotonicSumDataPoint::new(
                "requests",
                1,
                10,
                NumberValue::I64(8),
            ))
            .into_request()
            .unwrap();
        let metrics = &request.resource_metrics[0].scope_metrics[0].metrics;

        assert_eq!(metrics.len(), 1);
        let Some(proto::metric::Data::Sum(sum)) = &metrics[0].data else {
            panic!("expected sum");
        };
        assert_eq!(sum.data_points.len(), 2);
    }

    #[test]
    fn same_metric_name_with_conflicting_shapes_is_validation_error() {
        let error = MetricBatch::new(Resource::new(), InstrumentationScope::new("scope"))
            .with_monotonic_sum(MonotonicSumDataPoint::new(
                "requests",
                1,
                10,
                NumberValue::I64(7),
            ))
            .with_gauge(GaugeDataPoint::new("requests", 10, NumberValue::I64(7)))
            .into_request()
            .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::Validation);
    }

    #[test]
    fn empty_metric_name_is_validation_error() {
        let error = MetricBatch::new(Resource::new(), InstrumentationScope::new("scope"))
            .with_gauge(GaugeDataPoint::new("", 10, NumberValue::I64(1)))
            .into_request()
            .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::Validation);
    }

    #[test]
    fn reversed_monotonic_sum_interval_is_validation_error() {
        let error = MetricBatch::new(Resource::new(), InstrumentationScope::new("scope"))
            .with_monotonic_sum(MonotonicSumDataPoint::new(
                "requests",
                20,
                10,
                NumberValue::I64(7),
            ))
            .into_request()
            .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::Validation);
    }

    #[test]
    fn unsupported_metric_shapes_are_explicit_errors() {
        let error = MetricShape::Histogram.validate_supported().unwrap_err();

        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert_eq!(
            error.to_string(),
            "unsupported OpenTelemetry feature: histogram metrics"
        );
    }

    #[test]
    fn oversized_metric_request_returns_size_limit_error() {
        let error = batch()
            .encode_request(ExportLimits { max_encoded_len: 1 })
            .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::SizeLimit);
    }

    #[test]
    fn metrics_partial_success_decodes_rejections() {
        let response = proto::ExportMetricsServiceResponse {
            partial_success: Some(proto::ExportMetricsPartialSuccess {
                rejected_data_points: 2,
                error_message: "bad metrics".into(),
            }),
        };

        let result = MetricsExportResult::decode(&response.encode_to_vec()).unwrap();

        assert_eq!(result.rejected_data_points(), 2);
        assert_eq!(result.error_message(), "bad metrics");
    }

    #[test]
    fn empty_metric_export_does_not_write_to_transport() {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let mut client = MetricsClient::plaintext(
                client_fd,
                HttpConfig::default(),
                ExportClientConfig::default(),
            );
            let result = client
                .export(
                    cx,
                    MetricBatch::new(Resource::new(), InstrumentationScope::new("scope")),
                )
                .unwrap();
            let mut probe = [0_u8; 1];
            assert_eq!(
                recv(&server_fd, &mut probe, RecvFlags::DONTWAIT).unwrap_err(),
                rustix::io::Errno::AGAIN
            );
            assert_eq!(result.rejected_data_points(), 0);
        });
    }

    #[test]
    fn metrics_client_exports_sum_gauge_and_partial_success() {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            let mut client = MetricsClient::plaintext(
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
                        proto::ExportMetricsServiceRequest,
                        proto::ExportMetricsServiceResponse,
                        _,
                    >(METRICS_SERVICE_PATH, |_cx, _metadata, request| {
                        let metrics = &request.resource_metrics[0].scope_metrics[0].metrics;
                        assert_eq!(metrics.len(), 2);
                        assert_eq!(metrics[0].name, "requests");
                        assert!(matches!(metrics[0].data, Some(proto::metric::Data::Sum(_))));
                        assert_eq!(metrics[1].name, "temperature");
                        assert!(matches!(
                            metrics[1].data,
                            Some(proto::metric::Data::Gauge(_))
                        ));
                        Ok(UnaryReply::new(proto::ExportMetricsServiceResponse {
                            partial_success: Some(proto::ExportMetricsPartialSuccess {
                                rejected_data_points: 1,
                                error_message: "one rejected".into(),
                            }),
                        }))
                    });
                    grpc.serve_one(cx, &mut http).unwrap();
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let result = client.export(cx, batch()).unwrap();
                client.finish(cx).unwrap();
                server.join(cx);

                assert_eq!(result.rejected_data_points(), 1);
                assert_eq!(result.error_message(), "one rejected");
            });
        });
    }

    #[test]
    fn metrics_client_preserves_receiver_status_error_kind() {
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
                        proto::ExportMetricsServiceRequest,
                        proto::ExportMetricsServiceResponse,
                        _,
                    >(METRICS_SERVICE_PATH, |_cx, _metadata, _request| {
                        Err(Status::new(
                            StatusCode::Unavailable,
                            "collector unavailable",
                        ))
                    });
                    grpc.serve_one(cx, &mut http).unwrap();
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });
                let mut client = MetricsClient::plaintext(
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

        assert_eq!(error.kind(), ErrorKind::Status);
        assert!(error.to_string().contains("collector unavailable"));
    }

    #[test]
    fn blocked_metrics_export_allows_unrelated_stackful_work_to_continue() {
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
                    let mut client = MetricsClient::plaintext(
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
                        proto::ExportMetricsServiceRequest,
                        proto::ExportMetricsServiceResponse,
                        _,
                    >(METRICS_SERVICE_PATH, |_cx, _metadata, _request| {
                        Ok(UnaryReply::new(proto::ExportMetricsServiceResponse {
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

        assert_eq!(output.rejected_data_points(), 0);
    }

    #[test]
    fn allocation_metrics_export_warmed_local_loop_records_current_hot_path_allocations() {
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
                        proto::ExportMetricsServiceRequest,
                        proto::ExportMetricsServiceResponse,
                        _,
                    >(METRICS_SERVICE_PATH, |_cx, _metadata, _request| {
                        Ok(UnaryReply::new(proto::ExportMetricsServiceResponse {
                            partial_success: None,
                        }))
                    });
                    for _ in 0..WARMUP_ROUNDS + MEASURED_ROUNDS {
                        grpc.serve_one(cx, &mut http).unwrap();
                    }
                    http.shutdown_write_and_close_after_peer(cx).unwrap();
                });

                let mut client = MetricsClient::plaintext(
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
}
