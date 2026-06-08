// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::{
    Conditions, LeaseContext, MetadataMap, ObjectProperties, ObjectRef, OperationClass,
    RequestParts, page::parse_object_properties,
};

pub fn object_properties_request(
    object: &ObjectRef,
    conditions: Option<&Conditions>,
    lease: Option<&LeaseContext>,
) -> RequestParts {
    let mut request = RequestParts::new(OperationClass::Metadata, "HEAD", object_uri(object));
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

pub fn set_object_metadata_request(
    object: &ObjectRef,
    metadata: &MetadataMap,
    conditions: Option<&Conditions>,
    lease: Option<&LeaseContext>,
) -> RequestParts {
    let mut request = RequestParts::new(
        OperationClass::Metadata,
        "PUT",
        format!("{}?comp=metadata", object_uri(object)),
    );
    request.metadata.insert("content-length", "0");
    for (name, value) in metadata.entries() {
        request.metadata.insert(format!("x-ms-meta-{name}"), value);
    }
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

pub fn delete_object_request(
    object: &ObjectRef,
    conditions: Option<&Conditions>,
    lease: Option<&LeaseContext>,
) -> RequestParts {
    let mut request = RequestParts::new(OperationClass::Metadata, "DELETE", object_uri(object));
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

pub fn object_properties_from_headers(
    metadata: &MetadataMap,
) -> Result<ObjectProperties, crate::Error> {
    parse_object_properties(metadata)
}

pub(crate) fn object_uri(object: &ObjectRef) -> String {
    format!("/{}", object.encoded_path())
}

#[cfg(test)]
mod tests {
    use crate::{
        AccountId, ContainerName, ListOptions, ObjectKind, ObjectName, RequestCounts, list_request,
    };

    use super::*;

    fn object() -> ObjectRef {
        ObjectRef {
            account: AccountId::new("acct"),
            container: ContainerName::new("container"),
            name: ObjectName::new("object"),
            kind: ObjectKind::Metadata,
        }
    }

    #[test]
    fn property_requests_do_not_transfer_content_and_apply_conditions() {
        let lease = LeaseContext {
            lease_id: "lease".into(),
            epoch: 3,
        };
        let request =
            object_properties_request(&object(), Some(&Conditions::if_match("etag")), Some(&lease));

        assert_eq!(request.method, "HEAD");
        assert_eq!(request.metadata.get("if-match"), Some("etag"));
        assert_eq!(request.metadata.get("x-ms-lease-id"), Some("lease"));

        let mut metadata = MetadataMap::new();
        metadata.insert("high-water-lsn", "42");
        metadata.insert("relation-size", "1024");
        let set = set_object_metadata_request(
            &object(),
            &metadata,
            Some(&Conditions::if_none_match("*")),
            Some(&lease),
        );

        assert_eq!(set.method, "PUT");
        assert_eq!(set.metadata.get("content-length"), Some("0"));
        assert_eq!(set.metadata.get("x-ms-meta-high-water-lsn"), Some("42"));
        assert_eq!(set.metadata.get("x-ms-meta-relation-size"), Some("1024"));
        assert_eq!(set.metadata.get("if-none-match"), Some("*"));
        assert_eq!(set.metadata.get("x-ms-lease-id"), Some("lease"));

        let delete =
            delete_object_request(&object(), Some(&Conditions::if_match("etag")), Some(&lease));
        assert_eq!(delete.method, "DELETE");
        assert_eq!(delete.metadata.get("content-length"), Some("0"));
        assert_eq!(delete.metadata.get("if-match"), Some("etag"));
        assert_eq!(delete.metadata.get("x-ms-lease-id"), Some("lease"));
    }

    #[test]
    fn metadata_and_list_hot_paths_have_explicit_request_counts() {
        let mut counts = RequestCounts::default();
        let _properties = object_properties_request(&object(), None, None);
        counts.record_request(false);
        let _list = list_request(
            &AccountId::new("acct"),
            &ContainerName::new("container"),
            &ListOptions::default(),
        );
        counts.record_request(false);

        assert_eq!(counts.requests, 2);
        assert_eq!(counts.retries, 0);
        assert_eq!(counts.allocations, 0);
        assert_eq!(counts.allocated_bytes, 0);
    }
}
