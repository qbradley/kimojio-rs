// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::{BTreeMap, VecDeque};

use bytes::Bytes;
use kimojio_stack_storage::{
    AccountId, BlockDelete, BlockUpload, ContainerName, FakeResponse, FakeService, MetadataMap,
    ObjectKind, ObjectName, ObjectRef, ReplayBody, RequestParts, SignedSource, Transport,
    block_delete_request, block_download_request, block_upload_request, copy_from_source_request,
    list_request,
};

use super::model::{
    NamespaceMapping, ObjectErrorClass, ObjectKey, ObjectMetadata, ObjectOperation,
    ObjectSizeLimits, OperationOutcome, StorageFailureClass,
};

/// Gateway storage boundary implemented by hermetic and live backends.
pub trait GatewayStorage {
    fn put_stream<E>(
        &mut self,
        key: ObjectKey,
        content_type: impl Into<String>,
        next_chunk: impl FnMut() -> Result<Option<Bytes>, E>,
    ) -> Result<OperationOutcome<ObjectMetadata>, E>;
    fn get_object(&mut self, key: &ObjectKey) -> OperationOutcome<ObjectSnapshot>;
    fn delete(&mut self, key: &ObjectKey) -> OperationOutcome<bool>;
    fn list(&mut self, namespace: &str, prefix: &str) -> OperationOutcome<Vec<ObjectMetadata>>;
    fn copy(
        &mut self,
        source: &ObjectKey,
        destination: ObjectKey,
    ) -> OperationOutcome<ObjectMetadata>;
}

/// Stored object snapshot returned by storage for lazy response streaming.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectSnapshot {
    pub data: Bytes,
    pub metadata: ObjectMetadata,
}

#[derive(Clone, Debug)]
struct StoredObject {
    data: Bytes,
    metadata: ObjectMetadata,
}

/// In-process object store plus protocol-level request builder fixture.
#[derive(Debug)]
pub struct HermeticStorage {
    limits: ObjectSizeLimits,
    mapping: NamespaceMapping,
    generation: u64,
    objects: BTreeMap<ObjectKey, StoredObject>,
    requests: Vec<RequestParts>,
    injected: VecDeque<StorageFailureClass>,
}

impl HermeticStorage {
    pub fn new(limits: ObjectSizeLimits, mapping: NamespaceMapping) -> Self {
        Self {
            limits,
            mapping,
            generation: 0,
            objects: BTreeMap::new(),
            requests: Vec::new(),
            injected: VecDeque::new(),
        }
    }

    pub fn inject_failure(&mut self, failure: StorageFailureClass) {
        self.injected.push_back(failure);
    }

    pub fn requests(&self) -> &[RequestParts] {
        &self.requests
    }

    pub fn put_body(
        &mut self,
        key: ObjectKey,
        content_type: impl Into<String>,
        body: Bytes,
    ) -> OperationOutcome<ObjectMetadata> {
        if let Some(outcome) = self.maybe_fail(ObjectOperation::Put) {
            return outcome;
        }
        if let Err(error) = self.limits.classify_object(body.len() as u64) {
            return OperationOutcome::failure(ObjectOperation::Put, error);
        }
        self.record_and_store_body(key, content_type.into(), body)
    }

    pub fn put_chunks(
        &mut self,
        key: ObjectKey,
        content_type: impl Into<String>,
        chunks: Vec<Bytes>,
        total_len: usize,
    ) -> OperationOutcome<ObjectMetadata> {
        if let Some(outcome) = self.maybe_fail(ObjectOperation::Put) {
            return outcome;
        }
        if let Err(error) = self.limits.classify_object(total_len as u64) {
            return OperationOutcome::failure(ObjectOperation::Put, error);
        }
        let mut body = Vec::with_capacity(total_len);
        let mut total = 0u64;
        for chunk in chunks {
            if let Err(error) = self.limits.classify_chunk(chunk.len()) {
                return OperationOutcome::failure(ObjectOperation::Put, error);
            }
            total = total.saturating_add(chunk.len() as u64);
            if let Err(error) = self.limits.classify_object(total) {
                return OperationOutcome::failure(ObjectOperation::Put, error);
            }
            body.extend_from_slice(&chunk);
        }
        debug_assert_eq!(body.len(), total_len);
        self.record_and_store_body(key, content_type.into(), Bytes::from(body))
    }

    fn record_and_store_body(
        &mut self,
        key: ObjectKey,
        content_type: String,
        body: Bytes,
    ) -> OperationOutcome<ObjectMetadata> {
        let object = self.object_ref(&key);
        let metadata = MetadataMap::new();
        let request = match block_upload_request(BlockUpload {
            object: &object,
            body: ReplayBody::from_bytes(body.clone()),
            metadata: &metadata,
            lease: None,
            conditions: None,
            if_not_exists: false,
        }) {
            Ok(request) => request,
            Err(_) => {
                return OperationOutcome::failure(
                    ObjectOperation::Put,
                    ObjectErrorClass::StorageNonRetriable,
                );
            }
        };
        self.record(request);

        self.generation += 1;
        let metadata = ObjectMetadata {
            key: key.clone(),
            size_bytes: body.len() as u64,
            generation: self.generation,
            etag: format!("etag-{}", self.generation),
            content_type,
            user_metadata: BTreeMap::new(),
        };
        self.objects.insert(
            key,
            StoredObject {
                data: body,
                metadata: metadata.clone(),
            },
        );
        OperationOutcome::success(ObjectOperation::Put, metadata)
    }

    fn maybe_fail<T>(&mut self, operation: ObjectOperation) -> Option<OperationOutcome<T>> {
        self.injected
            .pop_front()
            .map(|failure| OperationOutcome::failure(operation, ObjectErrorClass::from(failure)))
    }

    fn object_ref(&self, key: &ObjectKey) -> ObjectRef {
        let location = self.mapping.storage_location(&key.namespace);
        let object_name = if location.prefix.is_empty() {
            key.object.as_str().to_owned()
        } else {
            format!("{}/{}", location.prefix, key.object.as_str())
        };
        ObjectRef {
            account: AccountId::new(location.account),
            container: ContainerName::new(location.container),
            name: ObjectName::new(object_name),
            kind: ObjectKind::Data,
        }
    }

    fn record(&mut self, request: RequestParts) {
        self.requests.push(request);
    }
}

impl GatewayStorage for HermeticStorage {
    fn put_stream<E>(
        &mut self,
        key: ObjectKey,
        content_type: impl Into<String>,
        mut next_chunk: impl FnMut() -> Result<Option<Bytes>, E>,
    ) -> Result<OperationOutcome<ObjectMetadata>, E> {
        if let Some(outcome) = self.maybe_fail(ObjectOperation::Put) {
            return Ok(outcome);
        }
        let mut total = 0u64;
        let mut body = Vec::new();
        while let Some(chunk) = next_chunk()? {
            if let Err(error) = self.limits.classify_chunk(chunk.len()) {
                return Ok(OperationOutcome::failure(ObjectOperation::Put, error));
            }
            total = total.saturating_add(chunk.len() as u64);
            if let Err(error) = self.limits.classify_object(total) {
                return Ok(OperationOutcome::failure(ObjectOperation::Put, error));
            }
            body.extend_from_slice(&chunk);
        }

        Ok(self.record_and_store_body(key, content_type.into(), Bytes::from(body)))
    }

    fn get_object(&mut self, key: &ObjectKey) -> OperationOutcome<ObjectSnapshot> {
        if let Some(outcome) = self.maybe_fail(ObjectOperation::Get) {
            return outcome;
        }
        let Some(stored) = self.objects.get(key).cloned() else {
            return OperationOutcome::failure(ObjectOperation::Get, ObjectErrorClass::NotFound);
        };
        let object = self.object_ref(key);
        self.record(block_download_request(&object, None));
        OperationOutcome::success(
            ObjectOperation::Get,
            ObjectSnapshot {
                data: stored.data,
                metadata: stored.metadata,
            },
        )
    }

    fn delete(&mut self, key: &ObjectKey) -> OperationOutcome<bool> {
        if let Some(outcome) = self.maybe_fail(ObjectOperation::Delete) {
            return outcome;
        }
        let object = self.object_ref(key);
        self.record(block_delete_request(BlockDelete {
            object: &object,
            snapshot: None,
            lease: None,
            conditions: None,
        }));
        if self.objects.remove(key).is_some() {
            OperationOutcome::success(ObjectOperation::Delete, true)
        } else {
            OperationOutcome::failure(ObjectOperation::Delete, ObjectErrorClass::NotFound)
        }
    }

    fn list(&mut self, namespace: &str, prefix: &str) -> OperationOutcome<Vec<ObjectMetadata>> {
        if let Some(outcome) = self.maybe_fail(ObjectOperation::List) {
            return outcome;
        }
        let namespace_id = super::model::NamespaceId::new(namespace);
        let location = self.mapping.storage_location(&namespace_id);
        let options = kimojio_stack_storage::ListOptions {
            prefix: Some(prefix.to_owned()).filter(|value| !value.is_empty()),
            include_metadata: true,
            ..kimojio_stack_storage::ListOptions::default()
        };
        self.record(list_request(
            &AccountId::new(location.account),
            &ContainerName::new(location.container),
            &options,
        ));
        let objects = self
            .objects
            .iter()
            .filter(|(key, _)| {
                key.namespace.as_str() == namespace && key.object.as_str().starts_with(prefix)
            })
            .map(|(_, stored)| stored.metadata.clone())
            .collect();
        OperationOutcome::success(ObjectOperation::List, objects)
    }

    fn copy(
        &mut self,
        source: &ObjectKey,
        destination: ObjectKey,
    ) -> OperationOutcome<ObjectMetadata> {
        if let Some(outcome) = self.maybe_fail(ObjectOperation::Copy) {
            return outcome;
        }
        let Some(source_object) = self.objects.get(source).cloned() else {
            return OperationOutcome::failure(ObjectOperation::Copy, ObjectErrorClass::NotFound);
        };
        let source_ref = self.object_ref(source);
        let destination_ref = self.object_ref(&destination);
        let source_uri = format!("https://example.invalid/{}", source_ref.path());
        let signed_source = SignedSource::new(source_uri);
        self.record(copy_from_source_request(
            &destination_ref,
            &signed_source,
            None,
        ));

        self.generation += 1;
        let mut metadata = source_object.metadata;
        metadata.key = destination.clone();
        metadata.generation = self.generation;
        metadata.etag = format!("etag-{}", self.generation);
        self.objects.insert(
            destination,
            StoredObject {
                data: source_object.data,
                metadata: metadata.clone(),
            },
        );
        OperationOutcome::success(ObjectOperation::Copy, metadata)
    }
}

/// Exercises the storage crate's `FakeService` transport with deterministic inputs.
pub fn fake_transport_round_trip() -> (RequestParts, Vec<Bytes>) {
    let mut fake = FakeService::new();
    fake.push_response(FakeResponse::ok(Bytes::from_static(b"body")));
    let request = RequestParts::new(kimojio_stack_storage::OperationClass::Block, "GET", "/obj");
    let mut chunks = Vec::new();
    kimojio_stack::Runtime::new()
        .block_on(|cx| {
            fake.execute_with_body_chunks(cx, &request, &mut |chunk| {
                chunks.push(chunk);
                Ok(())
            })
        })
        .unwrap();
    (fake.requests()[0].clone(), chunks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_gateway::model::NamespaceMapping;

    fn fixture() -> HermeticStorage {
        HermeticStorage::new(
            ObjectSizeLimits::new(16, 4),
            NamespaceMapping::HermeticContainerPerNamespace {
                account: "devstore".to_owned(),
            },
        )
    }

    fn put_test_stream(
        storage: &mut HermeticStorage,
        key: ObjectKey,
        content_type: &'static str,
        chunks: Vec<Bytes>,
    ) -> OperationOutcome<ObjectMetadata> {
        let mut chunks = chunks.into_iter();
        storage
            .put_stream(key, content_type, || {
                Ok::<_, std::convert::Infallible>(chunks.next())
            })
            .unwrap()
    }

    fn snapshot_chunks(snapshot: ObjectSnapshot, max_chunk_bytes: usize) -> Vec<Bytes> {
        snapshot
            .data
            .chunks(max_chunk_bytes)
            .map(Bytes::copy_from_slice)
            .collect()
    }

    #[test]
    fn object_gateway_storage_fixture_put_get_list_copy_delete() {
        let mut storage = fixture();
        let key = ObjectKey::new("ns", "a.txt");
        let put = put_test_stream(
            &mut storage,
            key.clone(),
            "text/plain",
            vec![Bytes::from_static(b"ab"), Bytes::from_static(b"cd")],
        );
        assert!(put.result.is_ok());

        let get = snapshot_chunks(storage.get_object(&key).result.unwrap(), 2);
        assert_eq!(
            get,
            vec![Bytes::from_static(b"ab"), Bytes::from_static(b"cd")]
        );

        let listed = storage.list("ns", "a").result.unwrap();
        assert_eq!(listed.len(), 1);

        let copied = storage
            .copy(&key, ObjectKey::new("ns", "copy.txt"))
            .result
            .unwrap();
        assert_eq!(copied.key.object.as_str(), "copy.txt");

        assert_eq!(storage.delete(&key).result, Ok(true));
        assert_eq!(
            storage.get_object(&key).result,
            Err(ObjectErrorClass::NotFound)
        );
        assert!(
            storage
                .requests()
                .iter()
                .any(|request| request.method == "PUT")
        );
        assert!(
            storage
                .requests()
                .iter()
                .any(|request| request.method == "GET")
        );
        assert!(
            storage
                .requests()
                .iter()
                .any(|request| request.uri.contains("comp=list"))
        );
        assert!(
            storage
                .requests()
                .iter()
                .any(|request| request.metadata.get("x-ms-copy-source").is_some())
        );
    }

    #[test]
    fn object_gateway_storage_fixture_enforces_bounded_chunks_and_objects() {
        let mut storage = fixture();
        assert_eq!(
            put_test_stream(
                &mut storage,
                ObjectKey::new("ns", "too-large-chunk"),
                "application/octet-stream",
                vec![Bytes::from_static(b"abcde")],
            )
            .result,
            Err(ObjectErrorClass::SizeLimit)
        );
        assert_eq!(
            put_test_stream(
                &mut storage,
                ObjectKey::new("ns", "too-large-object"),
                "application/octet-stream",
                vec![
                    Bytes::from_static(b"abcd"),
                    Bytes::from_static(b"abcd"),
                    Bytes::from_static(b"abcd"),
                    Bytes::from_static(b"abcd"),
                    Bytes::from_static(b"a"),
                ],
            )
            .result,
            Err(ObjectErrorClass::SizeLimit)
        );
    }

    #[test]
    fn object_gateway_storage_fixture_maps_injected_storage_failures() {
        let mut storage = fixture();
        storage.inject_failure(StorageFailureClass::Timeout);
        assert_eq!(
            put_test_stream(
                &mut storage,
                ObjectKey::new("ns", "timeout"),
                "application/octet-stream",
                vec![Bytes::from_static(b"ok")],
            )
            .result,
            Err(ObjectErrorClass::StorageTimeout)
        );
        storage.inject_failure(StorageFailureClass::Retriable);
        assert_eq!(
            storage.get_object(&ObjectKey::new("ns", "missing")).result,
            Err(ObjectErrorClass::StorageRetriable)
        );
        storage.inject_failure(StorageFailureClass::NonRetriable);
        assert_eq!(
            storage.delete(&ObjectKey::new("ns", "missing")).result,
            Err(ObjectErrorClass::StorageNonRetriable)
        );
    }

    #[test]
    fn object_gateway_storage_fixture_exercises_fake_transport_semantics() {
        let (request, chunks) = fake_transport_round_trip();
        assert_eq!(
            request.operation,
            kimojio_stack_storage::OperationClass::Block
        );
        assert_eq!(chunks, vec![Bytes::from_static(b"body")]);
    }
}
