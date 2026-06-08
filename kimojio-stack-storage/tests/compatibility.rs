// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use kimojio_stack_storage::{
    AccountEndpoint, AccountId, ArchiveDescriptor, BackupStatus, CompatibilityFixtures,
    Compression, Conditions, ContainerName, CopyInfo, Error, ErrorKind, LeaseContext, ObjectKind,
    ObjectName, ObjectRef, PageObjectInit, PageRange, PageWriteConditions, ReplayBody,
    RoutingTable, TenantInitPlan, TypedMetadata, archive_upload_request,
    classify_page_write_failure, config_request, copy_from_source_request, create_page_request,
    parse_delta_size, require_accepted_copy, set_ownership_epoch_request,
    upload_range_request_with_conditions,
};

#[test]
fn compatibility_fixtures_cover_operation_families_and_error_mapping() {
    let fixtures = CompatibilityFixtures::built_in();
    assert!(!fixtures.routing.is_empty());
    assert!(!fixtures.metadata.is_empty());
    assert!(!fixtures.errors.is_empty());

    for mapping in fixtures.errors {
        assert_eq!(
            Error::classify_status(mapping.status, Some(&mapping.service_code)),
            mapping.expected_kind
        );
    }
}

#[test]
fn compatibility_routing_fixtures_match_exact_accounts() {
    let accounts = [
        AccountEndpoint::new(AccountId::new("primary"), "p"),
        AccountEndpoint::new(AccountId::new("data0"), "d0"),
    ];
    let routing =
        RoutingTable::new(accounts[0].clone()).with_data_accounts(vec![accounts[1].clone()]);

    for fixture in CompatibilityFixtures::built_in().routing {
        let object = ObjectRef {
            account: AccountId::new("ignored"),
            container: ContainerName::new("container"),
            name: ObjectName::new(fixture.object_name),
            kind: fixture.kind,
        };
        assert_eq!(
            routing.route(&object).account,
            accounts[fixture.expected_account_index].account
        );
    }
}

fn data_object() -> ObjectRef {
    ObjectRef {
        account: AccountId::new("acct"),
        container: ContainerName::new("container"),
        name: ObjectName::new("data/page"),
        kind: ObjectKind::Data,
    }
}

fn typed_metadata() -> kimojio_stack_storage::MetadataMap {
    let mut metadata = TypedMetadata {
        high_water_lsn: Some(42),
        relation_size: Some(512),
        ownership_epoch: Some(7),
        ..TypedMetadata::default()
    }
    .to_metadata_map();
    metadata.insert("object-kind", "data");
    metadata
}

#[test]
fn compatibility_covers_page_and_ownership_semantics() {
    let object = data_object();
    let metadata = typed_metadata();

    let page_create = create_page_request(&object, 512, &metadata, None).unwrap();
    assert_eq!(page_create.metadata.get("if-none-match"), Some("*"));

    let page_write = upload_range_request_with_conditions(
        &object,
        PageRange::aligned_len(0, 512).unwrap(),
        ReplayBody::from_vec(vec![0; 512]),
        None,
        PageWriteConditions::default().with_sequence_number_eq(3),
    )
    .unwrap();
    assert_eq!(
        page_write.metadata.get("x-ms-if-sequence-number-eq"),
        Some("3")
    );
    assert!(matches!(
        classify_page_write_failure(&Error::new(ErrorKind::Unavailable, "ambiguous")),
        kimojio_stack_storage::PageWriteRecovery::RefreshPropertiesBeforeRetry
    ));

    let lease = LeaseContext {
        lease_id: "lease".into(),
        epoch: 8,
    };
    let epoch_update = set_ownership_epoch_request(&object, &lease, &metadata);
    assert_eq!(
        epoch_update.metadata.get("x-ms-meta-high-water-lsn"),
        Some("42")
    );
    assert_eq!(
        epoch_update.metadata.get("x-ms-meta-ownership-epoch"),
        Some("8")
    );
}

#[test]
fn compatibility_covers_archive_config_and_conditional_delete() {
    let object = data_object();

    let archive = archive_upload_request(
        &ArchiveDescriptor {
            object: ObjectRef {
                kind: ObjectKind::Backup,
                ..object.clone()
            },
            crc32: kimojio_stack_storage::crc32(b"backup"),
            compression: Compression::None,
        },
        ReplayBody::BorrowedStatic(b"backup"),
    )
    .unwrap();
    assert_eq!(
        archive.metadata.get("x-ms-meta-archive-compression"),
        Some("none")
    );

    let config = config_request(
        &ObjectRef {
            kind: ObjectKind::Config,
            name: ObjectName::new("config.json"),
            ..object.clone()
        },
        Some("etag"),
    );
    assert_eq!(config.metadata.get("if-none-match"), Some("etag"));

    let conditional = Conditions::if_match("etag");
    let conditional_request = kimojio_stack_storage::delete_object_request(
        &ObjectRef {
            account: AccountId::new("acct"),
            container: ContainerName::new("container"),
            name: ObjectName::new("metadata/root"),
            kind: ObjectKind::Metadata,
        },
        Some(&conditional),
        None,
    );
    assert_eq!(conditional_request.metadata.get("if-match"), Some("etag"));
}

#[test]
fn compatibility_covers_initialization_backup_snapshot_delta_and_copy() {
    let object = data_object();
    let metadata = typed_metadata();

    let init = TenantInitPlan {
        containers: vec![kimojio_stack_storage::ContainerTarget {
            account: AccountId::new("acct"),
            container: ContainerName::new("container"),
        }],
        metadata_object: ObjectRef {
            account: AccountId::new("acct"),
            container: ContainerName::new("container"),
            name: ObjectName::new("metadata/root"),
            kind: ObjectKind::Metadata,
        },
        metadata: metadata.clone(),
        page_objects: vec![PageObjectInit {
            object: ObjectRef {
                account: AccountId::new("acct"),
                container: ContainerName::new("container"),
                name: ObjectName::new("data/page"),
                kind: ObjectKind::Data,
            },
            content_len: 512,
        }],
    };
    assert_eq!(init.page_objects[0].content_len, 512);

    let status = BackupStatus {
        checkpoint_lsn: 99,
        state: "complete".into(),
    };
    assert_eq!(BackupStatus::parse(&status.to_bytes()).unwrap(), status);

    let snapshot = kimojio_stack_storage::create_snapshot_request(&object, None, None);
    assert_eq!(snapshot.metadata.get("content-length"), Some("0"));
    let mut delta = kimojio_stack_storage::MetadataMap::new();
    delta.insert("x-ms-delta-size", "1024");
    assert!(matches!(
        parse_delta_size(&delta).unwrap(),
        kimojio_stack_storage::DeltaSize::Available(1024)
    ));

    let copy = copy_from_source_request(
        &object,
        &kimojio_stack_storage::SignedSource::new("https://source/object"),
        None,
    );
    assert_eq!(
        copy.metadata.get("x-ms-copy-source"),
        Some("https://source/object")
    );
    assert!(
        require_accepted_copy(&CopyInfo {
            copy_id: Some("copy".into()),
            status: Some("pending".into()),
            request_id: Some("req".into()),
        })
        .is_ok()
    );
}
