// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::BTreeSet;

use crate::{AttemptError, SnapshotClient, SnapshotRef, Transport};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SnapshotProtection {
    protected: BTreeSet<(String, String)>,
}

impl SnapshotProtection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn protect(mut self, snapshot: &SnapshotRef) -> Self {
        self.protected.insert(snapshot_key(snapshot));
        self
    }

    pub fn is_protected(&self, snapshot: &SnapshotRef) -> bool {
        self.protected.contains(&snapshot_key(snapshot))
    }
}

pub fn orphan_snapshots(
    listed: impl IntoIterator<Item = SnapshotRef>,
    protection: &SnapshotProtection,
) -> Vec<SnapshotRef> {
    listed
        .into_iter()
        .filter(|snapshot| !protection.is_protected(snapshot))
        .collect()
}

pub fn delete_orphans_bounded<T: Transport>(
    cx: &kimojio_stack::RuntimeContext<'_>,
    transport: &mut T,
    orphans: &[SnapshotRef],
    max_deletes: usize,
) -> Result<usize, AttemptError> {
    let mut deleted = 0;
    for snapshot in orphans.iter().take(max_deletes) {
        SnapshotClient.delete(cx, transport, snapshot)?;
        deleted += 1;
    }
    Ok(deleted)
}

fn snapshot_key(snapshot: &SnapshotRef) -> (String, String) {
    (snapshot.object.path(), snapshot.snapshot.clone())
}

#[cfg(test)]
mod tests {
    use crate::{
        AccountId, ContainerName, Diagnostics, Error, MetadataMap, ObjectKind, ObjectName,
        RequestParts, ResponseParts,
    };

    use super::*;

    fn snapshot(id: &str) -> SnapshotRef {
        SnapshotRef {
            object: crate::ObjectRef {
                account: AccountId::new("acct"),
                container: ContainerName::new("container"),
                name: ObjectName::new("object"),
                kind: ObjectKind::Backup,
            },
            snapshot: id.into(),
        }
    }

    #[derive(Default)]
    struct DeleteTransport {
        requests: Vec<RequestParts>,
    }

    impl Transport for DeleteTransport {
        fn execute(
            &mut self,
            _cx: &kimojio_stack::RuntimeContext<'_>,
            request: &RequestParts,
        ) -> Result<ResponseParts, AttemptError> {
            self.requests.push(request.clone());
            Ok(ResponseParts {
                status: 202,
                metadata: MetadataMap::new(),
                diagnostics: Diagnostics::new(request.operation),
            })
        }

        fn execute_with_body_chunks(
            &mut self,
            _cx: &kimojio_stack::RuntimeContext<'_>,
            request: &RequestParts,
            _on_chunk: &mut dyn FnMut(bytes::Bytes) -> Result<(), Error>,
        ) -> Result<ResponseParts, AttemptError> {
            self.execute(_cx, request)
        }
    }

    #[test]
    fn orphan_snapshot_cleanup_discovers_and_deletes_with_bound() {
        let keep = snapshot("keep");
        let delete_a = snapshot("delete-a");
        let delete_b = snapshot("delete-b");
        let protection = SnapshotProtection::new().protect(&keep);
        let orphans = orphan_snapshots(
            vec![keep.clone(), delete_a.clone(), delete_b.clone()],
            &protection,
        );
        assert_eq!(orphans, vec![delete_a, delete_b]);

        let mut transport = DeleteTransport::default();
        let deleted = kimojio_stack::Runtime::new()
            .block_on(|cx| delete_orphans_bounded(cx, &mut transport, &orphans, 1))
            .unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(transport.requests.len(), 1);
    }
}
