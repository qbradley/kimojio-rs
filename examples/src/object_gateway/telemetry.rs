// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::model::{
    ObjectErrorClass, ObjectOperation, OperationOutcome, TelemetryDiagnosticOutcome,
};

use kimojio_stack_opentelemetry::{
    AnyValue, ExportLimits, GaugeDataPoint, InstrumentationScope, KeyValue, LogBatch, LogRecord,
    MetricBatch, NumberValue, Resource, SeverityNumber,
};

/// One operation-level log record expected from every gateway implementation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationLog {
    pub operation: ObjectOperation,
    pub error: Option<ObjectErrorClass>,
    pub message: String,
}

/// One operation-level metric record expected from every gateway implementation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationMetric {
    pub operation: ObjectOperation,
    pub error: Option<ObjectErrorClass>,
    pub count: u64,
}

/// Hermetic telemetry sink used by conformance and smoke tests.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HermeticTelemetrySink {
    logs: Vec<OperationLog>,
    metrics: Vec<OperationMetric>,
    encoded_log_exports: Vec<Vec<u8>>,
    encoded_metric_exports: Vec<Vec<u8>>,
    diagnostics: Vec<TelemetryDiagnosticOutcome>,
    fail_next_export: Option<String>,
}

impl HermeticTelemetrySink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn fail_next_export(&mut self, message: impl Into<String>) {
        self.fail_next_export = Some(message.into());
    }

    pub fn record<T>(&mut self, outcome: &mut OperationOutcome<T>) {
        let error = outcome.result.as_ref().err().copied();
        let log = OperationLog {
            operation: outcome.operation,
            error,
            message: outcome.operation.as_str().to_owned(),
        };
        let metric = OperationMetric {
            operation: outcome.operation,
            error,
            count: 1,
        };
        self.encoded_log_exports.push(encode_log_export(&log));
        self.encoded_metric_exports
            .push(encode_metric_export(&metric));
        self.logs.push(log);
        self.metrics.push(metric);
        if let Some(message) = self.fail_next_export.take() {
            let diagnostic = TelemetryDiagnosticOutcome::ExportFailed(message);
            outcome.telemetry.push(diagnostic.clone());
            self.diagnostics.push(diagnostic);
        } else {
            outcome.telemetry.push(TelemetryDiagnosticOutcome::Recorded);
            self.diagnostics.push(TelemetryDiagnosticOutcome::Recorded);
        }
    }

    pub fn logs(&self) -> &[OperationLog] {
        &self.logs
    }

    pub fn metrics(&self) -> &[OperationMetric] {
        &self.metrics
    }

    pub fn encoded_log_exports(&self) -> &[Vec<u8>] {
        &self.encoded_log_exports
    }

    pub fn encoded_metric_exports(&self) -> &[Vec<u8>] {
        &self.encoded_metric_exports
    }

    pub fn diagnostics(&self) -> &[TelemetryDiagnosticOutcome] {
        &self.diagnostics
    }

    pub fn has_log_and_metric(
        &self,
        operation: ObjectOperation,
        error: Option<ObjectErrorClass>,
    ) -> bool {
        self.logs
            .iter()
            .any(|log| log.operation == operation && log.error == error)
            && self
                .metrics
                .iter()
                .any(|metric| metric.operation == operation && metric.error == error)
    }
}

fn encode_log_export(log: &OperationLog) -> Vec<u8> {
    let batch = LogBatch::new(resource(), scope()).with_record(
        LogRecord::new(
            1,
            if log.error.is_some() {
                SeverityNumber::Warn
            } else {
                SeverityNumber::Info
            },
            AnyValue::String(log.message.clone()),
        )
        .with_attribute(KeyValue::new(
            "object_gateway.operation",
            AnyValue::String(log.operation.as_str().to_owned()),
        ))
        .with_attribute(KeyValue::new(
            "object_gateway.error",
            AnyValue::String(log.error.map(error_label).unwrap_or("none").to_owned()),
        )),
    );
    batch
        .encode_request(ExportLimits::default())
        .expect("hermetic operation log export stays within OTLP limits")
}

fn encode_metric_export(metric: &OperationMetric) -> Vec<u8> {
    let batch = MetricBatch::new(resource(), scope()).with_gauge(
        GaugeDataPoint::new(
            "object_gateway.operations",
            1,
            NumberValue::I64(metric.count as i64),
        )
        .with_attribute(KeyValue::new(
            "object_gateway.operation",
            AnyValue::String(metric.operation.as_str().to_owned()),
        ))
        .with_attribute(KeyValue::new(
            "object_gateway.error",
            AnyValue::String(metric.error.map(error_label).unwrap_or("none").to_owned()),
        )),
    );
    batch
        .encode_request(ExportLimits::default())
        .expect("hermetic operation metric export stays within OTLP limits")
}

fn resource() -> Resource {
    Resource::new().with_attribute(KeyValue::new(
        "service.name",
        AnyValue::String("object-gateway".to_owned()),
    ))
}

fn scope() -> InstrumentationScope {
    InstrumentationScope::new("examples.object_gateway")
}

fn error_label(error: ObjectErrorClass) -> &'static str {
    match error {
        ObjectErrorClass::NotFound => "not-found",
        ObjectErrorClass::SizeLimit => "size-limit",
        ObjectErrorClass::StorageTimeout => "storage-timeout",
        ObjectErrorClass::StorageRetriable => "storage-retriable",
        ObjectErrorClass::StorageNonRetriable => "storage-non-retriable",
        ObjectErrorClass::DeadlineExceeded => "deadline",
        ObjectErrorClass::Cancelled => "cancellation",
        ObjectErrorClass::Internal => "internal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_gateway_telemetry_records_success_and_failure_classes() {
        let mut sink = HermeticTelemetrySink::new();
        let mut success = OperationOutcome::success(ObjectOperation::Put, ());
        sink.record(&mut success);
        let mut failure =
            OperationOutcome::<()>::failure(ObjectOperation::Get, ObjectErrorClass::NotFound);
        sink.record(&mut failure);

        assert!(sink.has_log_and_metric(ObjectOperation::Put, None));
        assert!(sink.has_log_and_metric(ObjectOperation::Get, Some(ObjectErrorClass::NotFound)));
        assert_eq!(sink.logs().len(), 2);
        assert_eq!(sink.metrics().len(), 2);
        assert_eq!(
            sink.diagnostics(),
            &[
                TelemetryDiagnosticOutcome::Recorded,
                TelemetryDiagnosticOutcome::Recorded
            ]
        );
        assert_eq!(sink.encoded_log_exports().len(), 2);
        assert_eq!(sink.encoded_metric_exports().len(), 2);
        assert!(
            sink.encoded_log_exports()
                .iter()
                .all(|bytes| !bytes.is_empty())
        );
        assert!(
            sink.encoded_metric_exports()
                .iter()
                .all(|bytes| !bytes.is_empty())
        );
    }

    #[test]
    fn object_gateway_telemetry_export_failure_is_diagnostic_only() {
        let mut sink = HermeticTelemetrySink::new();
        sink.fail_next_export("collector unavailable");
        let mut outcome = OperationOutcome::success(ObjectOperation::Delete, true);
        sink.record(&mut outcome);

        assert_eq!(outcome.result, Ok(true));
        assert_eq!(
            outcome.telemetry,
            vec![TelemetryDiagnosticOutcome::ExportFailed(
                "collector unavailable".to_owned()
            )]
        );
        assert_eq!(sink.diagnostics(), outcome.telemetry.as_slice());
    }
}
