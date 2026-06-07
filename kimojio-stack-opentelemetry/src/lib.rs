// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Low-level OpenTelemetry Protocol export foundations for `kimojio-stack`.
//!
//! This crate intentionally exposes caller-controlled data construction and
//! export building blocks. It does not install a global provider, start
//! background workers, or perform implicit periodic export.

pub mod client;
pub mod error;
pub mod logs;
pub mod metrics;
mod proto;

pub use client::ExportClientConfig;
pub use error::{Error, ErrorKind};
pub use logs::{LogBatch, LogRecord, LogsClient, LogsExportResult, SeverityNumber};
pub use metrics::{
    GaugeDataPoint, MetricBatch, MetricShape, MetricsClient, MetricsExportResult,
    MonotonicSumDataPoint, NumberValue,
};

#[cfg(test)]
#[global_allocator]
static TEST_ALLOCATOR: allocation_tracking::CountingAllocator =
    allocation_tracking::CountingAllocator;

#[cfg(test)]
mod allocation_tracking {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
    }

    static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
    static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
    static REALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
    static REALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
    static MEASUREMENT_LOCK: Mutex<()> = Mutex::new(());

    pub struct CountingAllocator;

    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct AllocationCounts {
        pub allocations: usize,
        pub allocated_bytes: usize,
        pub reallocations: usize,
        pub reallocated_bytes: usize,
    }

    impl AllocationCounts {
        pub fn allocating_operations(self) -> usize {
            self.allocations + self.reallocations
        }

        pub fn allocated_or_reallocated_bytes(self) -> usize {
            self.allocated_bytes + self.reallocated_bytes
        }
    }

    pub fn measure<T>(f: impl FnOnce() -> T) -> (T, AllocationCounts) {
        let _measurement = MEASUREMENT_LOCK.lock().unwrap();
        ACTIVE.with(|active| assert!(!active.get(), "nested allocation measurement"));
        ALLOCATIONS.store(0, Ordering::Relaxed);
        ALLOCATED_BYTES.store(0, Ordering::Relaxed);
        REALLOCATIONS.store(0, Ordering::Relaxed);
        REALLOCATED_BYTES.store(0, Ordering::Relaxed);

        ACTIVE.with(|active| active.set(true));
        let output = f();
        ACTIVE.with(|active| active.set(false));

        (
            output,
            AllocationCounts {
                allocations: ALLOCATIONS.load(Ordering::Relaxed),
                allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
                reallocations: REALLOCATIONS.load(Ordering::Relaxed),
                reallocated_bytes: REALLOCATED_BYTES.load(Ordering::Relaxed),
            },
        )
    }

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            ACTIVE.with(|active| {
                if active.get() {
                    ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
                    ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
                }
            });
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            ACTIVE.with(|active| {
                if active.get() {
                    ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
                    ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
                }
            });
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            ACTIVE.with(|active| {
                if active.get() {
                    REALLOCATIONS.fetch_add(1, Ordering::Relaxed);
                    REALLOCATED_BYTES.fetch_add(new_size, Ordering::Relaxed);
                }
            });
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }
}

/// Caller-supplied resource attributes describing the telemetry producer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resource {
    attributes: Vec<KeyValue>,
}

impl Resource {
    /// Creates an empty resource.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an attribute and returns the updated resource.
    pub fn with_attribute(mut self, attribute: KeyValue) -> Self {
        self.attributes.push(attribute);
        self
    }

    /// Returns resource attributes.
    pub fn attributes(&self) -> &[KeyValue] {
        &self.attributes
    }

    fn into_proto(self) -> proto::Resource {
        proto::Resource {
            attributes: self
                .attributes
                .into_iter()
                .map(KeyValue::into_proto)
                .collect(),
        }
    }
}

/// Caller-supplied instrumentation-scope metadata.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InstrumentationScope {
    name: String,
    version: String,
    attributes: Vec<KeyValue>,
}

impl InstrumentationScope {
    /// Creates scope metadata with a name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Sets the scope version.
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Adds a scope attribute.
    pub fn with_attribute(mut self, attribute: KeyValue) -> Self {
        self.attributes.push(attribute);
        self
    }

    /// Returns the scope name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the scope version.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Returns scope attributes.
    pub fn attributes(&self) -> &[KeyValue] {
        &self.attributes
    }

    fn into_proto(self) -> proto::InstrumentationScope {
        proto::InstrumentationScope {
            name: self.name,
            version: self.version,
            attributes: self
                .attributes
                .into_iter()
                .map(KeyValue::into_proto)
                .collect(),
        }
    }
}

/// A telemetry attribute key/value pair.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyValue {
    key: String,
    value: AnyValue,
}

impl KeyValue {
    /// Creates a key/value pair.
    pub fn new(key: impl Into<String>, value: AnyValue) -> Self {
        Self {
            key: key.into(),
            value,
        }
    }

    /// Returns the attribute key.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Returns the attribute value.
    pub fn value(&self) -> &AnyValue {
        &self.value
    }

    fn into_proto(self) -> proto::KeyValue {
        proto::KeyValue {
            key: self.key,
            value: Some(self.value.into_proto()),
        }
    }
}

/// Supported low-level telemetry attribute values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnyValue {
    String(String),
    Bool(bool),
    I64(i64),
    Bytes(Vec<u8>),
}

impl AnyValue {
    fn into_proto(self) -> proto::AnyValue {
        use proto::any_value::Value;
        proto::AnyValue {
            value: Some(match self {
                Self::String(value) => Value::String(value),
                Self::Bool(value) => Value::Bool(value),
                Self::I64(value) => Value::Int(value),
                Self::Bytes(value) => Value::Bytes(value),
            }),
        }
    }
}

/// Shared export limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportLimits {
    /// Maximum encoded request bytes.
    pub max_encoded_len: usize,
}

impl Default for ExportLimits {
    fn default() -> Self {
        Self {
            max_encoded_len: 4 * 1024 * 1024,
        }
    }
}

fn validate_non_empty(value: &str, field: &'static str) -> Result<(), Error> {
    if value.is_empty() {
        Err(Error::Validation(field))
    } else {
        Ok(())
    }
}

fn check_encoded_len(len: usize, limits: ExportLimits) -> Result<(), Error> {
    if len > limits.max_encoded_len {
        Err(Error::SizeLimit {
            limit: limits.max_encoded_len,
            actual: len,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::*;

    #[test]
    fn resource_and_scope_attributes_encode_to_proto() {
        let resource = Resource::new().with_attribute(KeyValue::new(
            "service.name",
            AnyValue::String("svc".into()),
        ));
        let scope = InstrumentationScope::new("scope")
            .with_version("1.2.3")
            .with_attribute(KeyValue::new("enabled", AnyValue::Bool(true)));

        let resource = resource.into_proto();
        let scope = scope.into_proto();

        assert_eq!(resource.attributes.len(), 1);
        assert_eq!(resource.attributes[0].key, "service.name");
        assert_eq!(scope.name, "scope");
        assert_eq!(scope.version, "1.2.3");
        assert_eq!(scope.attributes.len(), 1);
    }

    #[test]
    fn attributes_support_string_bool_int_and_bytes() {
        let resource = Resource::new()
            .with_attribute(KeyValue::new("s", AnyValue::String("value".into())))
            .with_attribute(KeyValue::new("b", AnyValue::Bool(true)))
            .with_attribute(KeyValue::new("i", AnyValue::I64(42)))
            .with_attribute(KeyValue::new("bytes", AnyValue::Bytes(vec![1, 2, 3])));

        let encoded = resource.into_proto().encode_to_vec();
        assert!(!encoded.is_empty());
    }

    #[test]
    fn encoded_size_limit_is_inspectable() {
        let error = check_encoded_len(9, ExportLimits { max_encoded_len: 8 }).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::SizeLimit);
        assert_eq!(
            error.to_string(),
            "OpenTelemetry export size limit exceeded: limit 8, actual 9"
        );
    }
}

#[cfg(test)]
mod schema_validation_tests {
    use opentelemetry_proto::tonic::{
        collector::{logs::v1 as collector_logs, metrics::v1 as collector_metrics},
        common::v1 as common,
        logs::v1 as otlp_logs,
        metrics::v1 as otlp_metrics,
    };
    use prost::Message;

    use super::*;
    use crate::{
        logs::{LogBatch, LogRecord, LogsExportResult, SeverityNumber},
        metrics::{
            GaugeDataPoint, MetricBatch, MetricsExportResult, MonotonicSumDataPoint, NumberValue,
        },
    };

    #[test]
    fn logs_request_decodes_with_upstream_otlp_types() {
        let encoded = LogBatch::new(
            Resource::new().with_attribute(KeyValue::new(
                "service.name",
                AnyValue::String("svc".into()),
            )),
            InstrumentationScope::new("scope").with_version("1"),
        )
        .with_record(
            LogRecord::new(11, SeverityNumber::Info, AnyValue::String("body".into()))
                .with_observed_time_unix_nano(22)
                .with_severity_text("INFO")
                .with_attribute(KeyValue::new("answer", AnyValue::I64(42))),
        )
        .encode_request(ExportLimits::default())
        .unwrap();

        let decoded = collector_logs::ExportLogsServiceRequest::decode(encoded.as_slice()).unwrap();
        let resource_logs = &decoded.resource_logs[0];
        let scope_logs = &resource_logs.scope_logs[0];
        let record = &scope_logs.log_records[0];

        assert_eq!(
            resource_logs.resource.as_ref().unwrap().attributes[0].key,
            "service.name"
        );
        assert_eq!(scope_logs.scope.as_ref().unwrap().name, "scope");
        assert_eq!(record.time_unix_nano, 11);
        assert_eq!(record.observed_time_unix_nano, 22);
        assert_eq!(
            record.severity_number,
            otlp_logs::SeverityNumber::Info as i32
        );
        assert_eq!(record.severity_text, "INFO");
    }

    #[test]
    fn logs_partial_success_decodes_from_upstream_otlp_types() {
        let response = collector_logs::ExportLogsServiceResponse {
            partial_success: Some(collector_logs::ExportLogsPartialSuccess {
                rejected_log_records: 5,
                error_message: "rejected".into(),
            }),
        };

        let result = LogsExportResult::decode(&response.encode_to_vec()).unwrap();

        assert_eq!(result.rejected_log_records(), 5);
        assert_eq!(result.error_message(), "rejected");
    }

    #[test]
    fn metrics_request_decodes_with_upstream_otlp_types() {
        let encoded = MetricBatch::new(
            Resource::new().with_attribute(KeyValue::new(
                "service.name",
                AnyValue::String("svc".into()),
            )),
            InstrumentationScope::new("scope").with_version("1"),
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
        .encode_request(ExportLimits::default())
        .unwrap();

        let decoded =
            collector_metrics::ExportMetricsServiceRequest::decode(encoded.as_slice()).unwrap();
        let metrics = &decoded.resource_metrics[0].scope_metrics[0].metrics;

        assert_eq!(metrics[0].name, "requests");
        let Some(otlp_metrics::metric::Data::Sum(sum)) = &metrics[0].data else {
            panic!("expected sum metric");
        };
        assert_eq!(
            sum.aggregation_temporality,
            otlp_metrics::AggregationTemporality::Cumulative as i32
        );
        assert!(sum.is_monotonic);

        assert_eq!(metrics[1].name, "temperature");
        let Some(otlp_metrics::metric::Data::Gauge(gauge)) = &metrics[1].data else {
            panic!("expected gauge metric");
        };
        assert_eq!(
            gauge.data_points[0].value,
            Some(otlp_metrics::number_data_point::Value::AsDouble(42.5))
        );
    }

    #[test]
    fn metrics_partial_success_decodes_from_upstream_otlp_types() {
        let response = collector_metrics::ExportMetricsServiceResponse {
            partial_success: Some(collector_metrics::ExportMetricsPartialSuccess {
                rejected_data_points: 4,
                error_message: "bad metrics".into(),
            }),
        };

        let result = MetricsExportResult::decode(&response.encode_to_vec()).unwrap();

        assert_eq!(result.rejected_data_points(), 4);
        assert_eq!(result.error_message(), "bad metrics");
    }

    #[test]
    fn supported_attribute_values_decode_with_upstream_otlp_types() {
        let encoded = Resource::new()
            .with_attribute(KeyValue::new("s", AnyValue::String("value".into())))
            .with_attribute(KeyValue::new("b", AnyValue::Bool(true)))
            .with_attribute(KeyValue::new("i", AnyValue::I64(42)))
            .with_attribute(KeyValue::new("bytes", AnyValue::Bytes(vec![1, 2, 3])))
            .into_proto()
            .encode_to_vec();

        let decoded =
            opentelemetry_proto::tonic::resource::v1::Resource::decode(encoded.as_slice()).unwrap();

        assert_eq!(
            decoded.attributes[0].value.as_ref().unwrap().value,
            Some(common::any_value::Value::StringValue("value".into()))
        );
        assert_eq!(
            decoded.attributes[1].value.as_ref().unwrap().value,
            Some(common::any_value::Value::BoolValue(true))
        );
        assert_eq!(
            decoded.attributes[2].value.as_ref().unwrap().value,
            Some(common::any_value::Value::IntValue(42))
        );
        assert_eq!(
            decoded.attributes[3].value.as_ref().unwrap().value,
            Some(common::any_value::Value::BytesValue(vec![1, 2, 3]))
        );
    }
}
