// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[test]
fn real_storage_gate_is_explicit_for_snapshot_copy_delta_smoke() {
    if std::env::var("KIMOJIO_STACK_STORAGE_REAL_SMOKE").as_deref() != Ok("1") {
        eprintln!(
            "skipping real-storage smoke: set KIMOJIO_STACK_STORAGE_REAL_SMOKE=1 with credentials"
        );
        return;
    }

    let account = std::env::var("KIMOJIO_STACK_STORAGE_ACCOUNT")
        .expect("KIMOJIO_STACK_STORAGE_ACCOUNT must be set for real smoke tests");
    let container = std::env::var("KIMOJIO_STACK_STORAGE_CONTAINER")
        .expect("KIMOJIO_STACK_STORAGE_CONTAINER must be set for real smoke tests");

    assert!(!account.is_empty());
    assert!(!container.is_empty());

    let object = kimojio_stack_storage::ObjectRef {
        account: kimojio_stack_storage::AccountId::new(account),
        container: kimojio_stack_storage::ContainerName::new(container),
        name: kimojio_stack_storage::ObjectName::new("real-smoke"),
        kind: kimojio_stack_storage::ObjectKind::Backup,
    };
    let snapshot = kimojio_stack_storage::create_snapshot_request(&object, None, None);
    assert_eq!(snapshot.method, "PUT");
    let copy = kimojio_stack_storage::copy_from_source_request(
        &object,
        &kimojio_stack_storage::SignedSource::new("https://example.invalid/source"),
        None,
    );
    assert_eq!(
        copy.metadata.get("x-ms-copy-source"),
        Some("https://example.invalid/source")
    );
    let mut delta = kimojio_stack_storage::MetadataMap::new();
    delta.insert("x-ms-delta-size", "1");
    assert!(matches!(
        kimojio_stack_storage::parse_delta_size(&delta).unwrap(),
        kimojio_stack_storage::DeltaSize::Available(1)
    ));
}
