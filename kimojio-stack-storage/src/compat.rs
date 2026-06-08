// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::{ErrorKind, MetadataMap, ObjectKind, OperationClass};

/// Compatibility fixtures loaded by tests to preserve external storage semantics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompatibilityFixtures {
    pub routing: Vec<RoutingFixture>,
    pub metadata: Vec<MetadataFixture>,
    pub errors: Vec<ErrorMappingFixture>,
    pub caller_environments: Vec<CallerEnvironment>,
}

impl CompatibilityFixtures {
    /// Creates fixtures from caller-provided vectors.
    pub fn from_parts(
        routing: Vec<RoutingFixture>,
        metadata: Vec<MetadataFixture>,
        errors: Vec<ErrorMappingFixture>,
        caller_environments: Vec<CallerEnvironment>,
    ) -> Self {
        Self {
            routing,
            metadata,
            errors,
            caller_environments,
        }
    }

    /// Loads the built-in compatibility fixtures derived from the requirements.
    pub fn built_in() -> Self {
        Self::from_parts(
            vec![
                RoutingFixture {
                    object_name: "metadata/control".into(),
                    kind: ObjectKind::Metadata,
                    expected_account_index: 0,
                },
                RoutingFixture {
                    object_name: "data/tenant/relation/page".into(),
                    kind: ObjectKind::Data,
                    expected_account_index: 1,
                },
            ],
            vec![
                MetadataFixture {
                    key: "Highest_LSN".into(),
                    value: "42".into(),
                },
                MetadataFixture {
                    key: "lease_epoch".into(),
                    value: "7".into(),
                },
            ],
            vec![
                ErrorMappingFixture {
                    status: 404,
                    service_code: "BlobNotFound".into(),
                    operation: OperationClass::PageRead,
                    expected_kind: ErrorKind::NotFound,
                    retriable: false,
                },
                ErrorMappingFixture {
                    status: 412,
                    service_code: "SequenceNumberConditionNotMet".into(),
                    operation: OperationClass::PageWrite,
                    expected_kind: ErrorKind::SequenceNumber,
                    retriable: false,
                },
            ],
            vec![
                CallerEnvironment {
                    name: "page-writer".into(),
                    requires_async_bridge: false,
                },
                CallerEnvironment {
                    name: "archive-reader".into(),
                    requires_async_bridge: true,
                },
            ],
        )
    }
}

/// Golden routing expectation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutingFixture {
    pub object_name: String,
    pub kind: ObjectKind,
    pub expected_account_index: usize,
}

/// Metadata key/value expectation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataFixture {
    pub key: String,
    pub value: String,
}

impl MetadataFixture {
    /// Converts metadata fixtures into a normalized metadata map.
    pub fn into_map(fixtures: impl IntoIterator<Item = Self>) -> MetadataMap {
        let mut map = MetadataMap::new();
        for fixture in fixtures {
            map.insert(fixture.key, fixture.value);
        }
        map
    }
}

/// Service error mapping expectation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ErrorMappingFixture {
    pub status: u16,
    pub service_code: String,
    pub operation: OperationClass,
    pub expected_kind: ErrorKind,
    pub retriable: bool,
}

/// Current caller-environment descriptor used for integration parity tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallerEnvironment {
    pub name: String,
    pub requires_async_bridge: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_fixtures_hold_routing_error_and_environment_data() {
        let fixtures = CompatibilityFixtures::from_parts(
            vec![RoutingFixture {
                object_name: "data/object".into(),
                kind: ObjectKind::Data,
                expected_account_index: 2,
            }],
            vec![MetadataFixture {
                key: "Highest_LSN".into(),
                value: "42".into(),
            }],
            vec![ErrorMappingFixture {
                status: 412,
                service_code: "ConditionNotMet".into(),
                operation: OperationClass::Metadata,
                expected_kind: ErrorKind::ConditionNotMet,
                retriable: false,
            }],
            vec![CallerEnvironment {
                name: "primary".into(),
                requires_async_bridge: false,
            }],
        );

        assert_eq!(fixtures.routing[0].expected_account_index, 2);
        assert_eq!(
            MetadataFixture::into_map(fixtures.metadata.clone()).get("highest_lsn"),
            Some("42")
        );
        assert_eq!(fixtures.errors[0].expected_kind, ErrorKind::ConditionNotMet);
        assert_eq!(fixtures.caller_environments[0].name, "primary");
    }

    #[test]
    fn built_in_fixtures_are_non_empty_and_normalized() {
        let fixtures = CompatibilityFixtures::built_in();
        let metadata = MetadataFixture::into_map(fixtures.metadata.clone());

        assert!(
            fixtures
                .routing
                .iter()
                .any(|fixture| fixture.kind == ObjectKind::Data)
        );
        assert_eq!(metadata.get("HIGHEST_LSN"), Some("42"));
        assert!(fixtures.errors.iter().any(|fixture| {
            fixture.expected_kind == ErrorKind::SequenceNumber && !fixture.retriable
        }));
        assert!(
            fixtures
                .caller_environments
                .iter()
                .any(|environment| environment.requires_async_bridge)
        );
    }
}
