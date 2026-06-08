// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Archive object helpers.
//!
//! Archive support stores archive-specific metadata on block objects and streams
//! downloads through caller sinks while verifying CRC32. Compression codecs are
//! intentionally caller-owned; this module records whether bytes are stored
//! uncompressed or externally compressed.

use bytes::Bytes;
use std::sync::OnceLock;

use crate::{
    AttemptError, BlockClient, BlockUpload, Error, ErrorKind, MetadataMap, ObjectName, ObjectRef,
    ReplayBody, Transport, block_upload_request,
};

/// Archive compression metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Compression {
    /// Stored bytes are not compressed.
    None,
    /// Compression was applied outside this crate.
    External,
}

/// Descriptor for an archive object and its integrity metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveDescriptor {
    /// Archive object reference.
    pub object: ObjectRef,
    /// Expected IEEE CRC32 of the stored bytes.
    pub crc32: u32,
    /// Compression metadata advertised with the archive.
    pub compression: Compression,
}

/// Builds a block upload request with archive metadata.
pub fn archive_upload_request(
    descriptor: &ArchiveDescriptor,
    body: ReplayBody,
) -> Result<crate::RequestParts, Error> {
    let mut metadata = MetadataMap::new();
    metadata.insert("archive-crc32", format!("{:08x}", descriptor.crc32));
    metadata.insert(
        "archive-compression",
        match descriptor.compression {
            Compression::None => "none",
            Compression::External => "external",
        },
    );
    let mut request = block_upload_request(BlockUpload {
        object: &descriptor.object,
        body,
        metadata: &metadata,
        lease: None,
        conditions: None,
        if_not_exists: false,
    })?;
    request.operation = crate::OperationClass::Archive;
    Ok(request)
}

/// Validates bytes against an expected CRC32.
pub fn validate_archive_crc(bytes: &[u8], expected: u32) -> Result<(), Error> {
    let actual = crc32(bytes);
    if actual == expected {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::Corruption,
            format!("archive CRC mismatch: expected {expected:08x}, actual {actual:08x}"),
        ))
    }
}

/// Returns primary and fallback archive object names.
///
/// Fallback prefixes are joined with the primary name using one slash.
pub fn archive_candidates(primary: &ObjectName, fallback_prefixes: &[String]) -> Vec<ObjectName> {
    let mut candidates = Vec::with_capacity(fallback_prefixes.len() + 1);
    candidates.push(primary.clone());
    for prefix in fallback_prefixes {
        candidates.push(ObjectName::new(format!(
            "{}/{}",
            prefix.trim_end_matches('/'),
            primary.as_str()
        )));
    }
    candidates
}

/// Client helper for archive downloads with integrity validation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArchiveClient;

impl ArchiveClient {
    /// Downloads an archive, streams bytes to `on_chunk`, and validates CRC32.
    ///
    /// A CRC mismatch is returned after the complete body has been delivered. A
    /// mid-stream transport failure after any bytes have reached the sink remains
    /// an incomplete-download error from the block client.
    pub fn download_validate_to_sink<T, F>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        descriptor: &ArchiveDescriptor,
        mut on_chunk: F,
    ) -> Result<(), AttemptError>
    where
        T: Transport,
        F: FnMut(Bytes) -> Result<(), Error>,
    {
        let mut crc = Crc32::new();
        let mut delivered = 0_u64;
        match BlockClient.download(cx, transport, &descriptor.object, None, |chunk| {
            delivered += chunk.len() as u64;
            crc.update(&chunk);
            on_chunk(chunk)
        }) {
            Ok(_) => {}
            Err(error) if delivered != 0 && error.error.kind() == ErrorKind::Incomplete => {
                return Err(error);
            }
            Err(error) => return Err(error),
        }
        let actual = crc.finish();
        if actual == descriptor.crc32 {
            Ok(())
        } else {
            Err(AttemptError {
                error: Error::new(
                    ErrorKind::Corruption,
                    format!(
                        "archive CRC mismatch: expected {:08x}, actual {actual:08x}",
                        descriptor.crc32
                    ),
                ),
                diagnostics: crate::Diagnostics::new(crate::OperationClass::Archive),
            })
        }
    }
}

struct Crc32 {
    value: u32,
}

impl Crc32 {
    fn new() -> Self {
        Self { value: 0xffff_ffff }
    }

    fn update(&mut self, bytes: &[u8]) {
        let table = crc32_table();
        for byte in bytes {
            let index = ((self.value ^ u32::from(*byte)) & 0xff) as usize;
            self.value = (self.value >> 8) ^ table[index];
        }
    }

    fn finish(self) -> u32 {
        !self.value
    }
}

/// Computes IEEE CRC32 for bytes.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(bytes);
    crc.finish()
}

fn crc32_table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0_u32; 256];
        for (slot, value) in table.iter_mut().enumerate() {
            let mut crc = slot as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
            *value = crc;
        }
        table
    })
}

#[cfg(test)]
mod tests {
    use crate::{AccountId, ContainerName, ObjectKind, ObjectName, RequestParts, ResponseParts};

    use super::*;

    fn descriptor() -> ArchiveDescriptor {
        ArchiveDescriptor {
            object: ObjectRef {
                account: AccountId::new("acct"),
                container: ContainerName::new("container"),
                name: ObjectName::new("archive"),
                kind: ObjectKind::Backup,
            },
            crc32: crc32(b"abcdef"),
            compression: Compression::External,
        }
    }

    #[test]
    fn archive_upload_includes_crc_and_compression_metadata() {
        let request =
            archive_upload_request(&descriptor(), ReplayBody::BorrowedStatic(b"abcdef")).unwrap();
        let expected_crc = format!("{:08x}", crc32(b"abcdef"));

        assert_eq!(
            request.metadata.get("x-ms-meta-archive-crc32"),
            Some(expected_crc.as_str())
        );
        assert_eq!(
            request.metadata.get("x-ms-meta-archive-compression"),
            Some("external")
        );
        assert!(validate_archive_crc(b"abcdef", crc32(b"abcdef")).is_ok());
        assert_eq!(
            validate_archive_crc(b"abcdeg", crc32(b"abcdef"))
                .unwrap_err()
                .kind(),
            ErrorKind::Corruption
        );
    }

    #[test]
    fn archive_candidates_apply_prefix_fallbacks() {
        let candidates = archive_candidates(&ObjectName::new("rel/archive"), &["fallback".into()]);

        assert_eq!(candidates[0].as_str(), "rel/archive");
        assert_eq!(candidates[1].as_str(), "fallback/rel/archive");
    }

    #[derive(Default)]
    struct FakeTransport {
        chunks: Vec<Bytes>,
        requests: Vec<RequestParts>,
    }

    impl Transport for FakeTransport {
        fn execute(
            &mut self,
            cx: &kimojio_stack::RuntimeContext<'_>,
            request: &RequestParts,
        ) -> Result<ResponseParts, AttemptError> {
            let mut discard = |_| Ok(());
            self.execute_with_body_chunks(cx, request, &mut discard)
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
                    diagnostics: crate::Diagnostics::new(crate::OperationClass::Archive),
                })?;
            }
            Ok(ResponseParts {
                status: 200,
                metadata: MetadataMap::new(),
                diagnostics: crate::Diagnostics::new(crate::OperationClass::Archive),
            })
        }
    }

    #[test]
    fn archive_download_streams_to_sink_and_validates_crc() {
        let mut transport = FakeTransport {
            chunks: vec![Bytes::from_static(b"abc"), Bytes::from_static(b"def")],
            requests: Vec::new(),
        };
        let mut sink = Vec::new();

        kimojio_stack::Runtime::new()
            .block_on(|cx| {
                ArchiveClient.download_validate_to_sink(
                    cx,
                    &mut transport,
                    &descriptor(),
                    |chunk| {
                        sink.extend_from_slice(&chunk);
                        Ok(())
                    },
                )
            })
            .unwrap();

        assert_eq!(sink, b"abcdef");
        assert_eq!(transport.requests.len(), 1);
    }
}
