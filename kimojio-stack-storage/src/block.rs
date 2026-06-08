// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bytes::{Bytes, BytesMut};

use crate::{
    AttemptError, Conditions, Diagnostics, Error, ErrorKind, LeaseContext, MetadataMap, ObjectRef,
    OperationClass, ReplayBody, RequestParts, ResponseParts, Transport, ownership::apply_lease,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeleteOutcome {
    Deleted,
    NotFound,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockUploadOutcome {
    Uploaded,
    Skipped,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BlockClient;

impl BlockClient {
    pub fn upload<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        request: BlockUpload<'_>,
    ) -> Result<ResponseParts, AttemptError> {
        let request = block_upload_request(request).map_err(attempt_error)?;
        transport.execute(cx, &request)
    }

    pub fn upload_if_not_exists<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        mut request: BlockUpload<'_>,
    ) -> Result<BlockUploadOutcome, AttemptError> {
        request.if_not_exists = true;
        let request = block_upload_request(request).map_err(attempt_error)?;
        match transport.execute(cx, &request) {
            Ok(_) => Ok(BlockUploadOutcome::Uploaded),
            Err(error)
                if matches!(
                    error.error.kind(),
                    ErrorKind::AlreadyExists | ErrorKind::ConditionNotMet
                ) =>
            {
                Ok(BlockUploadOutcome::Skipped)
            }
            Err(error) => Err(error),
        }
    }

    pub fn download<T, F>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        object: &ObjectRef,
        range: Option<(u64, u64)>,
        mut on_chunk: F,
    ) -> Result<ResponseParts, AttemptError>
    where
        T: Transport,
        F: FnMut(Bytes) -> Result<(), Error>,
    {
        let request = block_download_request(object, range);
        let mut delivered = 0_u64;
        let mut sink_failed = false;
        match transport.execute_with_body_chunks(cx, &request, &mut |chunk| {
            let len = chunk.len() as u64;
            match on_chunk(chunk) {
                Ok(()) => {
                    delivered += len;
                    Ok(())
                }
                Err(error) => {
                    sink_failed = true;
                    Err(error)
                }
            }
        }) {
            Ok(response) => Ok(response),
            Err(error) if sink_failed => Err(error),
            Err(error) if delivered != 0 => Err(AttemptError {
                error: Error::new(
                    ErrorKind::Incomplete,
                    format!("block download failed after delivering {delivered} bytes"),
                ),
                diagnostics: error.diagnostics,
            }),
            Err(error) => Err(error),
        }
    }

    pub fn download_in_ranges<T, F>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        object: &ObjectRef,
        content_len: u64,
        chunk_size: u64,
        mut on_chunk: F,
    ) -> Result<(), AttemptError>
    where
        T: Transport,
        F: FnMut(Bytes) -> Result<(), Error>,
    {
        if chunk_size == 0 {
            return Err(attempt_error(Error::new(
                ErrorKind::Range,
                "download chunk size must be nonzero",
            )));
        }
        let mut offset = 0;
        while offset < content_len {
            let next = offset.checked_add(chunk_size).ok_or_else(|| {
                attempt_error(Error::new(ErrorKind::Range, "range offset overflow"))
            })?;
            let end = next.min(content_len).checked_sub(1).ok_or_else(|| {
                attempt_error(Error::new(ErrorKind::Range, "range end underflow"))
            })?;
            self.download(cx, transport, object, Some((offset, end)), &mut on_chunk)?;
            offset = end.checked_add(1).ok_or_else(|| {
                attempt_error(Error::new(ErrorKind::Range, "range progression overflow"))
            })?;
        }
        Ok(())
    }

    pub fn collect_small<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        object: &ObjectRef,
        max_bytes: usize,
    ) -> Result<Bytes, AttemptError> {
        let mut body = BytesMut::new();
        self.download(cx, transport, object, None, |chunk| {
            if body.len().saturating_add(chunk.len()) > max_bytes {
                return Err(Error::new(ErrorKind::Range, "small object limit exceeded"));
            }
            body.extend_from_slice(&chunk);
            Ok(())
        })?;
        Ok(body.freeze())
    }

    pub fn delete<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        request: BlockDelete<'_>,
    ) -> Result<DeleteOutcome, AttemptError> {
        let request = block_delete_request(request);
        match transport.execute(cx, &request) {
            Ok(_) => Ok(DeleteOutcome::Deleted),
            Err(error) if error.error.kind() == ErrorKind::NotFound => Ok(DeleteOutcome::NotFound),
            Err(error) => Err(error),
        }
    }
}

#[derive(Clone, Debug)]
pub struct BlockUpload<'a> {
    pub object: &'a ObjectRef,
    pub body: ReplayBody,
    pub metadata: &'a MetadataMap,
    pub lease: Option<&'a LeaseContext>,
    pub conditions: Option<&'a Conditions>,
    pub if_not_exists: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct BlockDelete<'a> {
    pub object: &'a ObjectRef,
    pub snapshot: Option<&'a str>,
    pub lease: Option<&'a LeaseContext>,
    pub conditions: Option<&'a Conditions>,
}

pub fn block_upload_request(input: BlockUpload<'_>) -> Result<RequestParts, Error> {
    let Some(len) = input.body.len() else {
        return Err(Error::new(
            ErrorKind::Transport,
            "block upload requires a known body length",
        ));
    };
    if input.body.as_bytes().is_none() {
        return Err(Error::new(
            ErrorKind::Transport,
            "block upload requires replayable bytes",
        ));
    }
    let mut request = RequestParts::new(OperationClass::Block, "PUT", object_uri(input.object));
    request.metadata.insert("content-length", len.to_string());
    request.metadata.insert("x-ms-blob-type", "BlockBlob");
    if input.if_not_exists {
        request.metadata.insert("if-none-match", "*");
    }
    for (name, value) in input.metadata.entries() {
        request.metadata.insert(format!("x-ms-meta-{name}"), value);
    }
    if let Some(conditions) = input.conditions {
        conditions.apply(&mut request);
    }
    if let Some(lease) = input.lease {
        apply_lease(&mut request, lease);
    }
    request.body = input.body;
    Ok(request)
}

pub fn block_download_request(object: &ObjectRef, range: Option<(u64, u64)>) -> RequestParts {
    let mut request = RequestParts::new(OperationClass::Block, "GET", object_uri(object));
    if let Some((start, end)) = range {
        request
            .metadata
            .insert("x-ms-range", format!("bytes={start}-{end}"));
    }
    request
}

pub fn block_delete_request(input: BlockDelete<'_>) -> RequestParts {
    let mut uri = object_uri(input.object);
    if let Some(snapshot) = input.snapshot {
        uri.push_str("?snapshot=");
        uri.push_str(&crate::list::percent_encode(snapshot));
    }
    let mut request = RequestParts::new(OperationClass::Block, "DELETE", uri);
    request.metadata.insert("content-length", "0");
    if let Some(conditions) = input.conditions {
        conditions.apply(&mut request);
    }
    if let Some(lease) = input.lease {
        apply_lease(&mut request, lease);
    }
    request
}

fn object_uri(object: &ObjectRef) -> String {
    format!("/{}", object.encoded_path())
}

fn attempt_error(error: Error) -> AttemptError {
    AttemptError {
        error,
        diagnostics: Diagnostics::new(OperationClass::Block),
    }
}

#[cfg(test)]
mod tests {
    use crate::{AccountId, ContainerName, ObjectKind, ObjectName};

    use super::*;

    #[derive(Default)]
    struct FakeTransport {
        requests: Vec<RequestParts>,
        chunks: Vec<Bytes>,
        response: Option<Result<ResponseParts, AttemptError>>,
    }

    impl Transport for FakeTransport {
        fn execute(
            &mut self,
            _cx: &kimojio_stack::RuntimeContext<'_>,
            request: &RequestParts,
        ) -> Result<ResponseParts, AttemptError> {
            self.requests.push(request.clone());
            self.response.clone().unwrap_or_else(ok_response)
        }

        fn execute_with_body_chunks(
            &mut self,
            _cx: &kimojio_stack::RuntimeContext<'_>,
            request: &RequestParts,
            on_chunk: &mut dyn FnMut(Bytes) -> Result<(), Error>,
        ) -> Result<ResponseParts, AttemptError> {
            self.requests.push(request.clone());
            for chunk in &self.chunks {
                on_chunk(chunk.clone()).map_err(attempt_error)?;
            }
            self.response.clone().unwrap_or_else(ok_response)
        }
    }

    fn ok_response() -> Result<ResponseParts, AttemptError> {
        Ok(ResponseParts {
            status: 200,
            metadata: MetadataMap::new(),
            diagnostics: Diagnostics::new(OperationClass::Archive),
        })
    }

    fn object() -> ObjectRef {
        ObjectRef {
            account: AccountId::new("acct"),
            container: ContainerName::new("container"),
            name: ObjectName::new("archive/object"),
            kind: ObjectKind::Backup,
        }
    }

    #[test]
    fn block_upload_if_not_exists_sets_body_metadata_conditions_and_lease() {
        let mut metadata = MetadataMap::new();
        metadata.insert("archive-crc32", "abcd");
        let lease = LeaseContext {
            lease_id: "lease".into(),
            epoch: 4,
        };
        let request = block_upload_request(BlockUpload {
            object: &object(),
            body: ReplayBody::from_vec(vec![1, 2, 3]),
            metadata: &metadata,
            lease: Some(&lease),
            conditions: Some(&Conditions::if_match("etag")),
            if_not_exists: true,
        })
        .unwrap();

        assert_eq!(request.method, "PUT");
        assert_eq!(request.metadata.get("content-length"), Some("3"));
        assert_eq!(request.metadata.get("if-none-match"), Some("*"));
        assert_eq!(request.metadata.get("if-match"), Some("etag"));
        assert_eq!(request.metadata.get("x-ms-lease-id"), Some("lease"));
        assert_eq!(
            request.metadata.get("x-ms-meta-archive-crc32"),
            Some("abcd")
        );
    }

    #[test]
    fn block_download_ranges_collect_small_and_delete_are_explicit() {
        let mut transport = FakeTransport {
            chunks: vec![Bytes::from_static(b"abc"), Bytes::from_static(b"def")],
            ..FakeTransport::default()
        };
        let collected = kimojio_stack::Runtime::new()
            .block_on(|cx| BlockClient.collect_small(cx, &mut transport, &object(), 8))
            .unwrap();
        assert_eq!(collected, Bytes::from_static(b"abcdef"));

        let mut transport = FakeTransport::default();
        kimojio_stack::Runtime::new()
            .block_on(|cx| {
                BlockClient.download_in_ranges(cx, &mut transport, &object(), 5, 2, |_| Ok(()))
            })
            .unwrap();
        assert_eq!(transport.requests.len(), 3);
        assert_eq!(
            transport.requests[0].metadata.get("x-ms-range"),
            Some("bytes=0-1")
        );
        assert_eq!(
            transport.requests[2].metadata.get("x-ms-range"),
            Some("bytes=4-4")
        );

        let delete = block_delete_request(BlockDelete {
            object: &object(),
            snapshot: Some("snap"),
            lease: None,
            conditions: Some(&Conditions::if_match("etag")),
        });
        assert!(delete.uri.contains("snapshot=snap"));
        assert_eq!(delete.metadata.get("content-length"), Some("0"));
        assert_eq!(delete.metadata.get("if-match"), Some("etag"));
    }

    #[test]
    fn block_delete_maps_missing_base_object_idempotently() {
        let mut transport = FakeTransport {
            response: Some(Err(attempt_error(Error::new(
                ErrorKind::NotFound,
                "missing",
            )))),
            ..FakeTransport::default()
        };
        let outcome = kimojio_stack::Runtime::new()
            .block_on(|cx| {
                BlockClient.delete(
                    cx,
                    &mut transport,
                    BlockDelete {
                        object: &object(),
                        snapshot: None,
                        lease: None,
                        conditions: None,
                    },
                )
            })
            .unwrap();

        assert_eq!(outcome, DeleteOutcome::NotFound);
    }

    #[test]
    fn block_upload_if_not_exists_maps_existing_object_to_skipped() {
        let mut transport = FakeTransport {
            response: Some(Err(attempt_error(Error::new(
                ErrorKind::ConditionNotMet,
                "exists",
            )))),
            ..FakeTransport::default()
        };
        let metadata = MetadataMap::new();
        let outcome = kimojio_stack::Runtime::new()
            .block_on(|cx| {
                BlockClient.upload_if_not_exists(
                    cx,
                    &mut transport,
                    BlockUpload {
                        object: &object(),
                        body: ReplayBody::BorrowedStatic(b"abc"),
                        metadata: &metadata,
                        lease: None,
                        conditions: None,
                        if_not_exists: false,
                    },
                )
            })
            .unwrap();

        assert_eq!(outcome, BlockUploadOutcome::Skipped);
        assert_eq!(
            transport.requests[0].metadata.get("if-none-match"),
            Some("*")
        );
    }

    #[test]
    fn block_range_download_rejects_overflowing_progression() {
        let mut transport = FakeTransport::default();
        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| {
                BlockClient.download_in_ranges(
                    cx,
                    &mut transport,
                    &object(),
                    u64::MAX,
                    u64::MAX - 1,
                    |_| Ok(()),
                )
            })
            .unwrap_err();

        assert_eq!(error.error.kind(), ErrorKind::Range);
    }

    #[test]
    fn block_download_reports_incomplete_after_partial_delivery() {
        let mut transport = FakeTransport {
            chunks: vec![Bytes::from_static(b"partial")],
            response: Some(Err(attempt_error(Error::new(
                ErrorKind::Unavailable,
                "busy",
            )))),
            ..FakeTransport::default()
        };
        let mut received = Vec::new();
        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| {
                BlockClient.download(cx, &mut transport, &object(), None, |chunk| {
                    received.extend_from_slice(&chunk);
                    Ok(())
                })
            })
            .unwrap_err();

        assert_eq!(received, b"partial");
        assert_eq!(error.error.kind(), ErrorKind::Incomplete);
    }
}
