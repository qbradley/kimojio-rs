// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ownership lease and epoch helpers.
//!
//! Ownership combines service leases with caller-maintained epoch metadata.
//! Request builders produce lease acquire/renew/change/break calls and metadata
//! updates; [`OwnershipWriteGuard`] classifies failed writes so callers know when
//! to retry, reacquire, or stop because their epoch is stale.

use crate::{
    Error, ErrorKind, LeaseContext, MetadataMap, ObjectRef, OperationClass, RequestParts,
    metadata::{OWNERSHIP_EPOCH, TypedMetadata},
};

/// Lease duration requested from the service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaseDuration {
    /// Infinite lease duration.
    Infinite,
    /// Finite lease duration in seconds.
    Seconds(u16),
}

impl LeaseDuration {
    fn as_header(self) -> String {
        match self {
            Self::Infinite => "-1".into(),
            Self::Seconds(seconds) => seconds.to_string(),
        }
    }
}

/// Decision after an ownership-protected write fails.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OwnershipWriteDecision {
    /// No recovery is needed; proceed.
    Proceed,
    /// Caller can retry according to its own policy.
    CallerRetry,
    /// Caller should reacquire ownership before retrying.
    ReacquireAndRetry,
    /// Failure is terminal.
    Terminal(Error),
}

/// Validates writes against the caller's expected ownership lease and epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnershipWriteGuard {
    expected: LeaseContext,
    allow_reacquire: bool,
}

impl OwnershipWriteGuard {
    /// Creates a guard for an expected lease context.
    pub fn new(expected: LeaseContext) -> Self {
        Self {
            expected,
            allow_reacquire: false,
        }
    }

    /// Allows ownership reacquire classification on lease/condition failures.
    pub fn with_reacquire(mut self, allow_reacquire: bool) -> Self {
        self.allow_reacquire = allow_reacquire;
        self
    }

    /// Returns the expected lease context.
    pub fn expected(&self) -> &LeaseContext {
        &self.expected
    }

    /// Validates response/object metadata against the expected ownership epoch.
    pub fn validate_metadata(&self, metadata: &MetadataMap) -> Result<(), Error> {
        validate_ownership_epoch(metadata, self.expected.epoch)
    }

    /// Classifies a failed ownership-protected write.
    ///
    /// If observed metadata contains a newer ownership epoch, the result is
    /// terminal even when reacquire is allowed.
    pub fn classify_failure(
        &self,
        error: &Error,
        observed_metadata: Option<&MetadataMap>,
    ) -> Result<OwnershipWriteDecision, Error> {
        if let Some(metadata) = observed_metadata {
            let typed = TypedMetadata::parse(metadata)?;
            if typed
                .ownership_epoch
                .is_some_and(|epoch| epoch > self.expected.epoch)
            {
                return Ok(OwnershipWriteDecision::Terminal(Error::new(
                    ErrorKind::Ownership,
                    "caller ownership epoch is stale",
                )));
            }
        }

        Ok(match error.kind() {
            ErrorKind::Ownership | ErrorKind::ConditionNotMet => {
                if self.allow_reacquire {
                    OwnershipWriteDecision::ReacquireAndRetry
                } else {
                    OwnershipWriteDecision::CallerRetry
                }
            }
            _ => OwnershipWriteDecision::Terminal(error.clone()),
        })
    }
}

/// Builds a lease-acquire request.
pub fn acquire_ownership_request(
    object: &ObjectRef,
    proposed_lease_id: Option<&str>,
    duration: LeaseDuration,
) -> RequestParts {
    let mut request = lease_request(object, "acquire");
    request
        .metadata
        .insert("x-ms-lease-duration", duration.as_header());
    if let Some(proposed_lease_id) = proposed_lease_id {
        request
            .metadata
            .insert("x-ms-proposed-lease-id", proposed_lease_id);
    }
    request
}

/// Builds a lease-renew request.
pub fn renew_ownership_request(object: &ObjectRef, lease: &LeaseContext) -> RequestParts {
    let mut request = lease_request(object, "renew");
    apply_lease(&mut request, lease);
    request
}

/// Builds a lease-change request.
pub fn change_ownership_request(
    object: &ObjectRef,
    current: &LeaseContext,
    proposed_lease_id: &str,
) -> RequestParts {
    let mut request = lease_request(object, "change");
    apply_lease(&mut request, current);
    request
        .metadata
        .insert("x-ms-proposed-lease-id", proposed_lease_id);
    request
}

/// Builds a metadata update request that persists the caller's ownership epoch.
pub fn set_ownership_epoch_request(
    object: &ObjectRef,
    lease: &LeaseContext,
    existing_metadata: &MetadataMap,
) -> RequestParts {
    let mut request = RequestParts::new(
        OperationClass::Metadata,
        "PUT",
        format!("/{}?comp=metadata", object.encoded_path()),
    );
    request.metadata.insert("content-length", "0");
    apply_lease(&mut request, lease);
    for (name, value) in existing_metadata.entries() {
        request.metadata.insert(format!("x-ms-meta-{name}"), value);
    }
    request.metadata.insert(
        format!("x-ms-meta-{OWNERSHIP_EPOCH}"),
        lease.epoch.to_string(),
    );
    request
}

/// Builds a lease-break request.
pub fn break_ownership_request(
    object: &ObjectRef,
    break_period_seconds: Option<u16>,
) -> RequestParts {
    let mut request = lease_request(object, "break");
    if let Some(seconds) = break_period_seconds {
        request
            .metadata
            .insert("x-ms-lease-break-period", seconds.to_string());
    }
    request
}

/// Validates that metadata contains exactly the expected ownership epoch.
pub fn validate_ownership_epoch(metadata: &MetadataMap, expected_epoch: u64) -> Result<(), Error> {
    let typed = TypedMetadata::parse(metadata)?;
    match typed.ownership_epoch {
        Some(epoch) if epoch == expected_epoch => Ok(()),
        Some(epoch) if epoch > expected_epoch => Err(Error::new(
            ErrorKind::Ownership,
            format!("observed ownership epoch {epoch} is newer than caller epoch {expected_epoch}"),
        )),
        Some(epoch) => Err(Error::new(
            ErrorKind::Ownership,
            format!(
                "observed ownership epoch {epoch} does not match caller epoch {expected_epoch}"
            ),
        )),
        None => Err(Error::new(
            ErrorKind::Ownership,
            "ownership epoch metadata is missing",
        )),
    }
}

/// Applies the lease ID header to a request.
pub fn apply_lease(request: &mut RequestParts, lease: &LeaseContext) {
    request
        .metadata
        .insert("x-ms-lease-id", lease.lease_id.as_str());
}

fn lease_request(object: &ObjectRef, action: &str) -> RequestParts {
    let mut request = RequestParts::new(
        OperationClass::Lease,
        "PUT",
        format!("/{}?comp=lease", object.encoded_path()),
    );
    request.metadata.insert("content-length", "0");
    request.metadata.insert("x-ms-lease-action", action);
    request
}

#[cfg(test)]
mod tests {
    use crate::{AccountId, ContainerName, ObjectKind, ObjectName};

    use super::*;

    fn object() -> ObjectRef {
        ObjectRef {
            account: AccountId::new("acct"),
            container: ContainerName::new("container"),
            name: ObjectName::new("object"),
            kind: ObjectKind::Data,
        }
    }

    #[test]
    fn ownership_requests_include_lease_action_epoch_and_ids() {
        let request =
            acquire_ownership_request(&object(), Some("lease-new"), LeaseDuration::Seconds(30));

        assert_eq!(request.method, "PUT");
        assert_eq!(request.uri, "/acct/container/object?comp=lease");
        assert_eq!(request.metadata.get("content-length"), Some("0"));
        assert_eq!(request.metadata.get("x-ms-lease-action"), Some("acquire"));
        assert_eq!(
            request.metadata.get("x-ms-proposed-lease-id"),
            Some("lease-new")
        );
        assert_eq!(request.metadata.get("x-ms-meta-ownership-epoch"), None);

        let current = LeaseContext {
            lease_id: "lease-old".into(),
            epoch: 7,
        };
        let change = change_ownership_request(&object(), &current, "lease-next");
        assert_eq!(change.metadata.get("x-ms-lease-id"), Some("lease-old"));
        assert_eq!(
            change.metadata.get("x-ms-proposed-lease-id"),
            Some("lease-next")
        );
        assert_eq!(change.metadata.get("x-ms-meta-ownership-epoch"), None);

        let mut existing = MetadataMap::new();
        existing.insert("high-water-lsn", "42");
        existing.insert(OWNERSHIP_EPOCH, "7");
        let epoch = set_ownership_epoch_request(
            &object(),
            &LeaseContext {
                lease_id: "lease-next".into(),
                epoch: 8,
            },
            &existing,
        );
        assert_eq!(epoch.uri, "/acct/container/object?comp=metadata");
        assert_eq!(epoch.metadata.get("content-length"), Some("0"));
        assert_eq!(epoch.metadata.get("x-ms-lease-id"), Some("lease-next"));
        assert_eq!(epoch.metadata.get("x-ms-meta-high-water-lsn"), Some("42"));
        assert_eq!(epoch.metadata.get("x-ms-meta-ownership-epoch"), Some("8"));
    }

    #[test]
    fn ownership_epoch_validation_detects_stale_callers() {
        let mut metadata = MetadataMap::new();
        metadata.insert(OWNERSHIP_EPOCH, "10");

        assert!(validate_ownership_epoch(&metadata, 10).is_ok());
        assert_eq!(
            validate_ownership_epoch(&metadata, 9).unwrap_err().kind(),
            ErrorKind::Ownership
        );
    }

    #[test]
    fn ownership_write_guard_classifies_retry_and_terminal_paths() {
        let guard = OwnershipWriteGuard::new(LeaseContext {
            lease_id: "lease".into(),
            epoch: 3,
        })
        .with_reacquire(true);

        assert_eq!(
            guard
                .classify_failure(&Error::new(ErrorKind::Ownership, "lost"), None)
                .unwrap(),
            OwnershipWriteDecision::ReacquireAndRetry
        );

        let mut metadata = MetadataMap::new();
        metadata.insert(OWNERSHIP_EPOCH, "4");
        assert!(matches!(
            guard
                .classify_failure(&Error::new(ErrorKind::Ownership, "lost"), Some(&metadata))
                .unwrap(),
            OwnershipWriteDecision::Terminal(_)
        ));
    }

    #[test]
    fn ownership_race_outcomes_cover_acquire_break_and_update_conflicts() {
        let guard_without_reacquire = OwnershipWriteGuard::new(LeaseContext {
            lease_id: "lease".into(),
            epoch: 3,
        });
        assert_eq!(
            guard_without_reacquire
                .classify_failure(&Error::new(ErrorKind::Ownership, "acquire raced"), None)
                .unwrap(),
            OwnershipWriteDecision::CallerRetry
        );

        let break_request = break_ownership_request(&object(), Some(5));
        assert_eq!(
            break_request.metadata.get("x-ms-lease-action"),
            Some("break")
        );
        assert_eq!(
            break_request.metadata.get("x-ms-lease-break-period"),
            Some("5")
        );

        let guard_with_reacquire = guard_without_reacquire.clone().with_reacquire(true);
        assert_eq!(
            guard_with_reacquire
                .classify_failure(&Error::new(ErrorKind::ConditionNotMet, "break raced"), None)
                .unwrap(),
            OwnershipWriteDecision::ReacquireAndRetry
        );

        let mut newer_metadata = MetadataMap::new();
        newer_metadata.insert(OWNERSHIP_EPOCH, "4");
        assert!(matches!(
            guard_with_reacquire
                .classify_failure(
                    &Error::new(ErrorKind::ConditionNotMet, "update raced"),
                    Some(&newer_metadata)
                )
                .unwrap(),
            OwnershipWriteDecision::Terminal(_)
        ));
    }
}
