// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::HashMap;

/// Fully-qualified protobuf package for the canonical object gateway contract.
pub const PACKAGE: &str = "kimojio.examples.object_gateway.v1";
/// Service name for the canonical object gateway contract.
pub const SERVICE_NAME: &str = "kimojio.examples.object_gateway.v1.ObjectGateway";
/// Client-streaming put RPC path.
pub const PUT_PATH: &str = "/kimojio.examples.object_gateway.v1.ObjectGateway/Put";
/// Server-streaming get RPC path.
pub const GET_PATH: &str = "/kimojio.examples.object_gateway.v1.ObjectGateway/Get";
/// Unary delete RPC path.
pub const DELETE_PATH: &str = "/kimojio.examples.object_gateway.v1.ObjectGateway/Delete";
/// Unary list RPC path.
pub const LIST_PATH: &str = "/kimojio.examples.object_gateway.v1.ObjectGateway/List";
/// Unary copy RPC path.
pub const COPY_PATH: &str = "/kimojio.examples.object_gateway.v1.ObjectGateway/Copy";
/// Unary health/status RPC path.
pub const HEALTH_PATH: &str = "/kimojio.examples.object_gateway.v1.ObjectGateway/Health";

/// Canonical service paths in the spec's six-operation order.
pub const SERVICE_PATHS: [&str; 6] = [
    PUT_PATH,
    GET_PATH,
    DELETE_PATH,
    LIST_PATH,
    COPY_PATH,
    HEALTH_PATH,
];

#[derive(Clone, PartialEq, prost::Message)]
pub struct ObjectIdentity {
    #[prost(string, tag = "1")]
    pub namespace: String,
    #[prost(string, tag = "2")]
    pub name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ObjectMetadata {
    #[prost(message, optional, tag = "1")]
    pub object: Option<ObjectIdentity>,
    #[prost(uint64, tag = "2")]
    pub size_bytes: u64,
    #[prost(uint64, tag = "3")]
    pub generation: u64,
    #[prost(string, tag = "4")]
    pub etag: String,
    #[prost(string, tag = "5")]
    pub content_type: String,
    #[prost(map = "string, string", tag = "6")]
    pub user_metadata: HashMap<String, String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OperationStatus {
    #[prost(enumeration = "ObjectStatusCode", tag = "1")]
    pub code: i32,
    #[prost(string, tag = "2")]
    pub message: String,
    #[prost(enumeration = "StorageStatusClass", tag = "3")]
    pub storage_class: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, prost::Enumeration)]
#[repr(i32)]
pub enum ObjectStatusCode {
    Ok = 0,
    NotFound = 1,
    SizeLimit = 2,
    StorageTimeout = 3,
    StorageRetriable = 4,
    StorageNonRetriable = 5,
    DeadlineExceeded = 6,
    Cancelled = 7,
    Internal = 8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, prost::Enumeration)]
#[repr(i32)]
pub enum StorageStatusClass {
    None = 0,
    Timeout = 1,
    Retriable = 2,
    NonRetriable = 3,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PutChunk {
    #[prost(message, optional, tag = "1")]
    pub object: Option<ObjectIdentity>,
    #[prost(uint64, tag = "2")]
    pub offset: u64,
    #[prost(bytes = "bytes", tag = "3")]
    pub data: bytes::Bytes,
    #[prost(bool, tag = "4")]
    pub finish: bool,
    #[prost(string, tag = "5")]
    pub content_type: String,
    #[prost(map = "string, string", tag = "6")]
    pub user_metadata: HashMap<String, String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PutResponse {
    #[prost(message, optional, tag = "1")]
    pub status: Option<OperationStatus>,
    #[prost(message, optional, tag = "2")]
    pub metadata: Option<ObjectMetadata>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GetRequest {
    #[prost(message, optional, tag = "1")]
    pub object: Option<ObjectIdentity>,
    #[prost(uint32, tag = "2")]
    pub max_chunk_bytes: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GetChunk {
    #[prost(message, optional, tag = "1")]
    pub status: Option<OperationStatus>,
    #[prost(message, optional, tag = "2")]
    pub metadata: Option<ObjectMetadata>,
    #[prost(uint64, tag = "3")]
    pub offset: u64,
    #[prost(bytes = "bytes", tag = "4")]
    pub data: bytes::Bytes,
    #[prost(bool, tag = "5")]
    pub finish: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DeleteRequest {
    #[prost(message, optional, tag = "1")]
    pub object: Option<ObjectIdentity>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DeleteResponse {
    #[prost(message, optional, tag = "1")]
    pub status: Option<OperationStatus>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ListRequest {
    #[prost(string, tag = "1")]
    pub namespace: String,
    #[prost(string, tag = "2")]
    pub prefix: String,
    #[prost(uint32, tag = "3")]
    pub max_results: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ListResponse {
    #[prost(message, optional, tag = "1")]
    pub status: Option<OperationStatus>,
    #[prost(message, repeated, tag = "2")]
    pub objects: Vec<ObjectMetadata>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CopyRequest {
    #[prost(message, optional, tag = "1")]
    pub source: Option<ObjectIdentity>,
    #[prost(message, optional, tag = "2")]
    pub destination: Option<ObjectIdentity>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CopyResponse {
    #[prost(message, optional, tag = "1")]
    pub status: Option<OperationStatus>,
    #[prost(message, optional, tag = "2")]
    pub metadata: Option<ObjectMetadata>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HealthRequest {}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HealthResponse {
    #[prost(bool, tag = "1")]
    pub ready: bool,
    #[prost(bool, tag = "2")]
    pub shutting_down: bool,
    #[prost(uint64, tag = "3")]
    pub in_flight: u64,
    #[prost(uint64, tag = "4")]
    pub completed: u64,
    #[prost(uint64, tag = "5")]
    pub failed: u64,
    #[prost(string, tag = "6")]
    pub version: String,
}

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::*;

    #[test]
    fn object_gateway_proto_paths_match_canonical_service() {
        assert_eq!(PACKAGE, "kimojio.examples.object_gateway.v1");
        assert_eq!(SERVICE_PATHS.len(), 6);
        assert_eq!(PUT_PATH, format!("/{SERVICE_NAME}/Put"));
        assert_eq!(GET_PATH, format!("/{SERVICE_NAME}/Get"));
        assert_eq!(DELETE_PATH, format!("/{SERVICE_NAME}/Delete"));
        assert_eq!(LIST_PATH, format!("/{SERVICE_NAME}/List"));
        assert_eq!(COPY_PATH, format!("/{SERVICE_NAME}/Copy"));
        assert_eq!(HEALTH_PATH, format!("/{SERVICE_NAME}/Health"));
    }

    #[test]
    fn object_gateway_proto_messages_round_trip() {
        let mut metadata = HashMap::new();
        metadata.insert("owner".to_owned(), "test".to_owned());
        let response = PutResponse {
            status: Some(OperationStatus {
                code: ObjectStatusCode::Ok as i32,
                message: String::new(),
                storage_class: StorageStatusClass::None as i32,
            }),
            metadata: Some(ObjectMetadata {
                object: Some(ObjectIdentity {
                    namespace: "ns".to_owned(),
                    name: "obj".to_owned(),
                }),
                size_bytes: 5,
                generation: 7,
                etag: "etag-7".to_owned(),
                content_type: "application/octet-stream".to_owned(),
                user_metadata: metadata,
            }),
        };

        let mut encoded = Vec::new();
        response.encode(&mut encoded).unwrap();
        let decoded = PutResponse::decode(encoded.as_slice()).unwrap();

        assert_eq!(decoded, response);
    }

    #[test]
    fn object_gateway_proto_stream_chunks_round_trip_bytes() {
        let chunk = PutChunk {
            object: Some(ObjectIdentity {
                namespace: "ns".to_owned(),
                name: "large".to_owned(),
            }),
            offset: 1024,
            data: bytes::Bytes::from_static(b"payload"),
            finish: false,
            content_type: "application/octet-stream".to_owned(),
            user_metadata: HashMap::new(),
        };
        let encoded = chunk.encode_to_vec();
        let decoded = PutChunk::decode(encoded.as_slice()).unwrap();

        assert_eq!(decoded.offset, 1024);
        assert_eq!(decoded.data, bytes::Bytes::from_static(b"payload"));
        assert!(!decoded.finish);
    }
}
