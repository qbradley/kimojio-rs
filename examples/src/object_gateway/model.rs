// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::BTreeMap;
use std::fmt;

/// The six object service operations in the canonical contract.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ObjectOperation {
    Put,
    Get,
    Delete,
    List,
    Copy,
    Health,
}

impl ObjectOperation {
    pub const ALL: [Self; 6] = [
        Self::Put,
        Self::Get,
        Self::Delete,
        Self::List,
        Self::Copy,
        Self::Health,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Put => "put",
            Self::Get => "get",
            Self::Delete => "delete",
            Self::List => "list",
            Self::Copy => "copy",
            Self::Health => "health",
        }
    }
}

/// Logical object namespace used by conformance scenarios.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct NamespaceId(String);

impl NamespaceId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NamespaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Object name inside a namespace.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ObjectId(String);

impl ObjectId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Full object key used by the shared model and hermetic storage fixture.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ObjectKey {
    pub namespace: NamespaceId,
    pub object: ObjectId,
}

impl ObjectKey {
    pub fn new(namespace: impl Into<String>, object: impl Into<String>) -> Self {
        Self {
            namespace: NamespaceId::new(namespace),
            object: ObjectId::new(object),
        }
    }
}

/// Metadata visible through put/get/list/copy responses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectMetadata {
    pub key: ObjectKey,
    pub size_bytes: u64,
    pub generation: u64,
    pub etag: String,
    pub content_type: String,
    pub user_metadata: BTreeMap<String, String>,
}

/// Configured object and streaming chunk size limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectSizeLimits {
    pub max_object_bytes: u64,
    pub max_chunk_bytes: usize,
}

impl ObjectSizeLimits {
    pub const fn new(max_object_bytes: u64, max_chunk_bytes: usize) -> Self {
        Self {
            max_object_bytes,
            max_chunk_bytes,
        }
    }

    pub fn classify_chunk(self, len: usize) -> Result<(), ObjectErrorClass> {
        if len > self.max_chunk_bytes {
            Err(ObjectErrorClass::SizeLimit)
        } else {
            Ok(())
        }
    }

    pub fn classify_object(self, len: u64) -> Result<(), ObjectErrorClass> {
        if len > self.max_object_bytes {
            Err(ObjectErrorClass::SizeLimit)
        } else {
            Ok(())
        }
    }
}

/// Deterministic object-visible error classes used by conformance.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ObjectErrorClass {
    NotFound,
    SizeLimit,
    StorageTimeout,
    StorageRetriable,
    StorageNonRetriable,
    DeadlineExceeded,
    Cancelled,
    Internal,
}

/// Storage-specific failure classes before object-visible mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageFailureClass {
    NotFound,
    Timeout,
    Retriable,
    NonRetriable,
}

impl From<StorageFailureClass> for ObjectErrorClass {
    fn from(value: StorageFailureClass) -> Self {
        match value {
            StorageFailureClass::NotFound => Self::NotFound,
            StorageFailureClass::Timeout => Self::StorageTimeout,
            StorageFailureClass::Retriable => Self::StorageRetriable,
            StorageFailureClass::NonRetriable => Self::StorageNonRetriable,
        }
    }
}

/// Telemetry export state kept separate from object-visible results.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TelemetryDiagnosticOutcome {
    Recorded,
    ExportFailed(String),
}

/// One operation outcome in conformance output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationOutcome<T> {
    pub operation: ObjectOperation,
    pub result: Result<T, ObjectErrorClass>,
    pub telemetry: Vec<TelemetryDiagnosticOutcome>,
}

impl<T> OperationOutcome<T> {
    pub fn success(operation: ObjectOperation, value: T) -> Self {
        Self {
            operation,
            result: Ok(value),
            telemetry: Vec::new(),
        }
    }

    pub fn failure(operation: ObjectOperation, error: ObjectErrorClass) -> Self {
        Self {
            operation,
            result: Err(error),
            telemetry: Vec::new(),
        }
    }

    pub fn with_telemetry(mut self, diagnostic: TelemetryDiagnosticOutcome) -> Self {
        self.telemetry.push(diagnostic);
        self
    }
}

/// How a logical namespace maps to a storage backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NamespaceMapping {
    HermeticContainerPerNamespace {
        account: String,
    },
    LiveContainerPrefix {
        account: String,
        container: String,
        prefix: String,
    },
}

impl NamespaceMapping {
    pub fn storage_location(&self, namespace: &NamespaceId) -> StorageLocation {
        match self {
            Self::HermeticContainerPerNamespace { account } => StorageLocation {
                account: account.clone(),
                container: namespace.as_str().to_owned(),
                prefix: String::new(),
            },
            Self::LiveContainerPrefix {
                account,
                container,
                prefix,
            } => StorageLocation {
                account: account.clone(),
                container: container.clone(),
                prefix: format!("{}/{}", prefix.trim_matches('/'), namespace.as_str()),
            },
        }
    }
}

/// Concrete storage location derived from namespace mapping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageLocation {
    pub account: String,
    pub container: String,
    pub prefix: String,
}

/// Runtime implementation label for normalized workload output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeMode {
    Stack,
    StackSteal,
    Tokio,
    Go,
}

impl RuntimeMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stack => "stack",
            Self::StackSteal => "stack-steal",
            Self::Tokio => "tokio",
            Self::Go => "go",
        }
    }
}

/// Storage backend mode label for normalized workload output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageBackendMode {
    Hermetic,
    LiveAzureCompatible,
}

impl StorageBackendMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hermetic => "hermetic",
            Self::LiveAzureCompatible => "live-azure-compatible",
        }
    }
}

/// Telemetry sink mode label for normalized workload output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetrySinkMode {
    Hermetic,
    Otlp,
}

impl TelemetrySinkMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hermetic => "hermetic",
            Self::Otlp => "otlp",
        }
    }
}

/// Labels emitted with workload rows so runtime and SDK costs stay separable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkloadLabels {
    pub implementation: String,
    pub runtime_mode: RuntimeMode,
    pub storage_client: String,
    pub storage_backend: StorageBackendMode,
    pub telemetry_sink: TelemetrySinkMode,
    pub workload: String,
}

/// Expected result for one deterministic conformance case.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConformanceExpectation {
    pub operation: ObjectOperation,
    pub expected_error: Option<ObjectErrorClass>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_gateway_model_exposes_exactly_six_operations() {
        let names = ObjectOperation::ALL.map(ObjectOperation::as_str);
        assert_eq!(names, ["put", "get", "delete", "list", "copy", "health"]);
    }

    #[test]
    fn object_gateway_model_classifies_size_limits() {
        let limits = ObjectSizeLimits::new(8, 4);
        assert_eq!(limits.classify_chunk(4), Ok(()));
        assert_eq!(limits.classify_chunk(5), Err(ObjectErrorClass::SizeLimit));
        assert_eq!(limits.classify_object(8), Ok(()));
        assert_eq!(limits.classify_object(9), Err(ObjectErrorClass::SizeLimit));
    }

    #[test]
    fn object_gateway_model_maps_storage_failures_deterministically() {
        assert_eq!(
            ObjectErrorClass::from(StorageFailureClass::Timeout),
            ObjectErrorClass::StorageTimeout
        );
        assert_eq!(
            ObjectErrorClass::from(StorageFailureClass::Retriable),
            ObjectErrorClass::StorageRetriable
        );
        assert_eq!(
            ObjectErrorClass::from(StorageFailureClass::NonRetriable),
            ObjectErrorClass::StorageNonRetriable
        );
    }

    #[test]
    fn object_gateway_model_namespace_mapping_is_explicit() {
        let hermetic = NamespaceMapping::HermeticContainerPerNamespace {
            account: "devstore".to_owned(),
        }
        .storage_location(&NamespaceId::new("case-1"));
        assert_eq!(hermetic.container, "case-1");
        assert!(hermetic.prefix.is_empty());

        let live = NamespaceMapping::LiveContainerPrefix {
            account: "acct".to_owned(),
            container: "objects".to_owned(),
            prefix: "runs".to_owned(),
        }
        .storage_location(&NamespaceId::new("case-1"));
        assert_eq!(live.container, "objects");
        assert_eq!(live.prefix, "runs/case-1");
    }

    #[test]
    fn object_gateway_model_telemetry_diagnostics_do_not_change_result() {
        let outcome = OperationOutcome::success(ObjectOperation::Put, 7u64).with_telemetry(
            TelemetryDiagnosticOutcome::ExportFailed("collector unavailable".to_owned()),
        );

        assert_eq!(outcome.result, Ok(7));
        assert_eq!(outcome.telemetry.len(), 1);
    }
}
