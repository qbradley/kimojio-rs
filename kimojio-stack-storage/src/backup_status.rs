// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bytes::{Bytes, BytesMut};
use serde_json::{Value, json};

use crate::{
    AttemptError, BlockClient, BlockUpload, Error, ErrorKind, MetadataMap, ObjectRef,
    OperationClass, ReplayBody, Transport,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupStatus {
    pub checkpoint_lsn: u64,
    pub state: String,
}

impl BackupStatus {
    pub fn to_bytes(&self) -> Bytes {
        Bytes::from(
            json!({
                "checkpoint_lsn": self.checkpoint_lsn,
                "state": self.state,
            })
            .to_string(),
        )
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let value = serde_json::from_slice::<Value>(bytes)
            .map_err(|error| Error::new(ErrorKind::Corruption, error.to_string()))?;
        let checkpoint_lsn = value
            .get("checkpoint_lsn")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::new(ErrorKind::Corruption, "checkpoint_lsn missing"))?;
        let state = value
            .get("state")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::new(ErrorKind::Corruption, "state missing"))?
            .to_owned();
        Ok(Self {
            checkpoint_lsn,
            state,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BackupStatusClient;

impl BackupStatusClient {
    pub fn update<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        object: &ObjectRef,
        status: &BackupStatus,
    ) -> Result<(), AttemptError> {
        let mut metadata = MetadataMap::new();
        metadata.insert("status-kind", "checkpoint");
        BlockClient.upload(
            cx,
            transport,
            BlockUpload {
                object,
                body: ReplayBody::from_bytes(status.to_bytes()),
                metadata: &metadata,
                lease: None,
                conditions: None,
                if_not_exists: false,
            },
        )?;
        Ok(())
    }

    pub fn read<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        object: &ObjectRef,
    ) -> Result<BackupStatus, AttemptError> {
        let mut body = BytesMut::new();
        BlockClient.download(cx, transport, object, None, |chunk| {
            body.extend_from_slice(&chunk);
            Ok(())
        })?;
        BackupStatus::parse(&body).map_err(|error| AttemptError {
            error,
            diagnostics: crate::Diagnostics::new(OperationClass::Config),
        })
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use crate::{
        AccountId, ContainerName, Diagnostics, ObjectKind, ObjectName, RequestParts, ResponseParts,
    };

    use super::*;

    #[derive(Default)]
    struct FakeTransport {
        requests: Vec<RequestParts>,
        chunks: Vec<Bytes>,
    }

    impl Transport for FakeTransport {
        fn execute(
            &mut self,
            _cx: &kimojio_stack::RuntimeContext<'_>,
            request: &RequestParts,
        ) -> Result<ResponseParts, AttemptError> {
            self.requests.push(request.clone());
            Ok(ResponseParts {
                status: 200,
                metadata: MetadataMap::new(),
                diagnostics: Diagnostics::new(request.operation),
            })
        }

        fn execute_with_body_chunks(
            &mut self,
            _cx: &kimojio_stack::RuntimeContext<'_>,
            request: &RequestParts,
            on_chunk: &mut dyn FnMut(Bytes) -> Result<(), Error>,
        ) -> Result<ResponseParts, AttemptError> {
            self.requests.push(request.clone());
            for chunk in &self.chunks {
                on_chunk(chunk.clone()).map_err(|error| AttemptError {
                    error,
                    diagnostics: Diagnostics::new(request.operation),
                })?;
            }
            Ok(ResponseParts {
                status: 200,
                metadata: MetadataMap::new(),
                diagnostics: Diagnostics::new(request.operation),
            })
        }
    }

    fn object() -> ObjectRef {
        ObjectRef {
            account: AccountId::new("acct"),
            container: ContainerName::new("container"),
            name: ObjectName::new("status"),
            kind: ObjectKind::Status,
        }
    }

    #[test]
    fn backup_status_updates_and_reads_back_json() {
        let status = BackupStatus {
            checkpoint_lsn: 42,
            state: "running".into(),
        };
        let mut transport = FakeTransport::default();
        kimojio_stack::Runtime::new()
            .block_on(|cx| BackupStatusClient.update(cx, &mut transport, &object(), &status))
            .unwrap();
        assert_eq!(
            transport.requests[0].metadata.get("x-ms-meta-status-kind"),
            Some("checkpoint")
        );

        let mut transport = FakeTransport {
            chunks: vec![status.to_bytes()],
            requests: Vec::new(),
        };
        let read = kimojio_stack::Runtime::new()
            .block_on(|cx| BackupStatusClient.read(cx, &mut transport, &object()))
            .unwrap();
        assert_eq!(read, status);
    }
}
