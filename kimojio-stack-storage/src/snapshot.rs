// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot lifecycle helpers.
//!
//! Snapshots are addressed by a base object plus snapshot identifier. Helpers
//! build create/delete/property/list requests, poll for delta-size metadata, and
//! convert list items with snapshot IDs back into [`SnapshotRef`] values.

use crate::{
    AttemptError, Conditions, Error, ErrorKind, LeaseContext, MetadataMap, ObjectProperties,
    ObjectRef, OperationClass, RequestParts, Transport, properties::object_uri,
};

/// Reference to one object snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotRef {
    /// Base object.
    pub object: ObjectRef,
    /// Service snapshot identifier.
    pub snapshot: String,
}

/// Delta-size availability for a snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeltaSize {
    /// Delta size has not appeared yet.
    Pending,
    /// Delta size is available in bytes.
    Available(u64),
}

/// Client helper for snapshot lifecycle operations.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SnapshotClient;

impl SnapshotClient {
    /// Creates a snapshot and returns its service identifier.
    pub fn create<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        object: &ObjectRef,
        conditions: Option<&Conditions>,
        lease: Option<&LeaseContext>,
    ) -> Result<SnapshotRef, AttemptError> {
        let response =
            transport.execute(cx, &create_snapshot_request(object, conditions, lease))?;
        let snapshot = response.metadata.get("x-ms-snapshot").ok_or_else(|| {
            attempt_error(Error::new(ErrorKind::Corruption, "snapshot id missing"))
        })?;
        Ok(SnapshotRef {
            object: object.clone(),
            snapshot: snapshot.to_owned(),
        })
    }

    /// Deletes a snapshot, treating already-absent snapshots as success.
    pub fn delete<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        snapshot: &SnapshotRef,
    ) -> Result<(), AttemptError> {
        match transport.execute(cx, &delete_snapshot_request(snapshot)) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Polls snapshot properties until delta size is available or attempts are exhausted.
    ///
    /// This method does not sleep between polls; callers that need backoff should
    /// perform it around repeated property calls.
    pub fn poll_delta_size<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        snapshot: &SnapshotRef,
        max_polls: usize,
    ) -> Result<DeltaSize, AttemptError> {
        let mut last = DeltaSize::Pending;
        for _ in 0..max_polls {
            let response = transport.execute(cx, &snapshot_properties_request(snapshot))?;
            last = parse_delta_size(&response.metadata).map_err(attempt_error)?;
            if matches!(last, DeltaSize::Available(_)) {
                break;
            }
        }
        Ok(last)
    }
}

/// Builds a create-snapshot request.
pub fn create_snapshot_request(
    object: &ObjectRef,
    conditions: Option<&Conditions>,
    lease: Option<&LeaseContext>,
) -> RequestParts {
    let mut request = RequestParts::new(
        OperationClass::Snapshot,
        "PUT",
        format!("{}?comp=snapshot", object_uri(object)),
    );
    request.metadata.insert("content-length", "0");
    if let Some(conditions) = conditions {
        conditions.apply(&mut request);
    }
    if let Some(lease) = lease {
        request
            .metadata
            .insert("x-ms-lease-id", lease.lease_id.as_str());
    }
    request
}

/// Builds a delete-snapshot request.
pub fn delete_snapshot_request(snapshot: &SnapshotRef) -> RequestParts {
    let mut request = RequestParts::new(OperationClass::Snapshot, "DELETE", snapshot_uri(snapshot));
    request.metadata.insert("content-length", "0");
    request
}

/// Builds a snapshot properties request.
pub fn snapshot_properties_request(snapshot: &SnapshotRef) -> RequestParts {
    RequestParts::new(OperationClass::Snapshot, "HEAD", snapshot_uri(snapshot))
}

/// Builds a request that lists snapshots for an object prefix.
pub fn list_snapshots_request(object: &ObjectRef) -> RequestParts {
    RequestParts::new(
        OperationClass::Snapshot,
        "GET",
        format!(
            "/{}/{}?restype=container&comp=list&prefix={}&include=snapshots,metadata",
            crate::model::percent_encode_component(object.account.as_str(), false),
            crate::model::percent_encode_component(object.container.as_str(), false),
            crate::list::percent_encode(object.name.as_str())
        ),
    )
}

/// Converts list items with snapshot identifiers into snapshot references.
pub fn snapshot_refs_from_list_items(
    base: &ObjectRef,
    items: &[crate::ListItem],
) -> Vec<SnapshotRef> {
    items
        .iter()
        .filter_map(|item| {
            item.snapshot.as_ref().map(|snapshot| SnapshotRef {
                object: ObjectRef {
                    name: item.name.clone(),
                    ..base.clone()
                },
                snapshot: snapshot.clone(),
            })
        })
        .collect()
}

/// Parses snapshot properties from response metadata.
pub fn parse_snapshot_properties(metadata: &MetadataMap) -> Result<ObjectProperties, Error> {
    crate::page::parse_object_properties(metadata)
}

/// Parses delta-size metadata.
pub fn parse_delta_size(metadata: &MetadataMap) -> Result<DeltaSize, Error> {
    if let Some(value) = metadata.get("x-ms-delta-size") {
        let size = value
            .parse::<u64>()
            .map_err(|_| Error::new(ErrorKind::MetadataFormat, "invalid delta size"))?;
        if size != 0 {
            return Ok(DeltaSize::Available(size));
        }
    }
    Ok(DeltaSize::Pending)
}

/// Returns the source reference URI for copy-from-snapshot operations.
pub fn snapshot_source_reference(snapshot: &SnapshotRef) -> String {
    format!(
        "{}?snapshot={}",
        object_uri(&snapshot.object),
        crate::list::percent_encode(&snapshot.snapshot)
    )
}

fn snapshot_uri(snapshot: &SnapshotRef) -> String {
    snapshot_source_reference(snapshot)
}

fn attempt_error(error: Error) -> AttemptError {
    AttemptError {
        error,
        diagnostics: crate::Diagnostics::new(OperationClass::Snapshot),
    }
}

#[cfg(test)]
mod tests {
    use crate::{AccountId, ContainerName, Diagnostics, ObjectKind, ObjectName, ResponseParts};

    use super::*;

    fn object() -> ObjectRef {
        ObjectRef {
            account: AccountId::new("acct"),
            container: ContainerName::new("container"),
            name: ObjectName::new("object"),
            kind: ObjectKind::Backup,
        }
    }

    #[test]
    fn snapshot_requests_cover_lifecycle_and_source_reference() {
        let snapshot = SnapshotRef {
            object: object(),
            snapshot: "snap".into(),
        };
        let create = create_snapshot_request(&object(), Some(&Conditions::if_match("etag")), None);
        assert_eq!(create.metadata.get("content-length"), Some("0"));
        assert_eq!(create.metadata.get("if-match"), Some("etag"));
        assert!(
            delete_snapshot_request(&snapshot)
                .uri
                .contains("snapshot=snap")
        );
        assert_eq!(snapshot_properties_request(&snapshot).method, "HEAD");
        assert!(
            list_snapshots_request(&object())
                .uri
                .contains("include=snapshots")
        );
        let special = ObjectRef {
            name: ObjectName::new("a b&c"),
            ..object()
        };
        assert!(
            list_snapshots_request(&special)
                .uri
                .contains("prefix=a%20b%26c")
        );
        assert_eq!(
            snapshot_source_reference(&snapshot),
            "/acct/container/object?snapshot=snap"
        );
    }

    #[test]
    fn delta_size_parses_pending_and_available() {
        let mut pending = MetadataMap::new();
        pending.insert("x-ms-copy-progress", "1/2");
        assert_eq!(parse_delta_size(&pending).unwrap(), DeltaSize::Pending);

        let mut available = MetadataMap::new();
        available.insert("x-ms-delta-size", "4096");
        assert_eq!(
            parse_delta_size(&available).unwrap(),
            DeltaSize::Available(4096)
        );
        let mut zero = MetadataMap::new();
        zero.insert("x-ms-delta-size", "0");
        assert_eq!(parse_delta_size(&zero).unwrap(), DeltaSize::Pending);
        zero.insert("x-ms-copy-progress", "1/2");
        zero.insert("x-ms-delta-size", "2048");
        assert_eq!(parse_delta_size(&zero).unwrap(), DeltaSize::Available(2048));
    }

    #[derive(Default)]
    struct PollTransport {
        responses: Vec<ResponseParts>,
    }

    impl Transport for PollTransport {
        fn execute(
            &mut self,
            _cx: &kimojio_stack::RuntimeContext<'_>,
            _request: &RequestParts,
        ) -> Result<ResponseParts, AttemptError> {
            Ok(self.responses.remove(0))
        }
    }

    #[test]
    fn snapshot_client_polls_delta_size_until_available() {
        let snapshot = SnapshotRef {
            object: object(),
            snapshot: "snap".into(),
        };
        let mut pending = MetadataMap::new();
        pending.insert("x-ms-copy-progress", "1/2");
        let mut available = MetadataMap::new();
        available.insert("x-ms-delta-size", "2048");
        let mut transport = PollTransport {
            responses: vec![
                ResponseParts {
                    status: 200,
                    metadata: pending,
                    diagnostics: Diagnostics::new(OperationClass::Snapshot),
                },
                ResponseParts {
                    status: 200,
                    metadata: available,
                    diagnostics: Diagnostics::new(OperationClass::Snapshot),
                },
            ],
        };

        let delta = kimojio_stack::Runtime::new()
            .block_on(|cx| SnapshotClient.poll_delta_size(cx, &mut transport, &snapshot, 2))
            .unwrap();
        assert_eq!(delta, DeltaSize::Available(2048));
    }

    #[test]
    fn snapshot_refs_are_built_from_list_items() {
        let page = crate::parse_list_page(
            "<EnumerationResults><Blobs><Blob><Name>a&amp;b</Name><Snapshot>snap</Snapshot><Metadata><object-kind>backup</object-kind></Metadata></Blob></Blobs></EnumerationResults>",
            Some(ObjectKind::Backup),
        )
        .unwrap();
        let items = page.items;

        let snapshots = snapshot_refs_from_list_items(&object(), &items);
        assert_eq!(snapshots[0].snapshot, "snap");
        assert_eq!(snapshots[0].object.name.as_str(), "a&b");
    }
}
