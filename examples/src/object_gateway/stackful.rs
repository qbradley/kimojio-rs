// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use bytes::Bytes;
use http::{Response, StatusCode, header::CONTENT_TYPE};
use kimojio_stack::RuntimeSocket;
use kimojio_stack_grpc::{
    ClientStreamingRequest, GrpcRuntime, Metadata, RuntimeUnaryClient, RuntimeUnaryServer,
    ServerConfig, ServerStreamingReply, Status, StatusCode as GrpcStatusCode, UnaryReply,
    UnaryResponse, server::ServerStream,
};
use kimojio_stack_http::{
    Body, BodyLimits, HttpConfig, HttpRuntime, RuntimeStackTransport, h2, http1,
};

use super::{
    admin::AdminStatus,
    model::{
        NamespaceMapping, ObjectErrorClass, ObjectKey, ObjectMetadata, ObjectOperation,
        ObjectSizeLimits, OperationOutcome,
    },
    proto,
    storage::{GatewayStorage, HermeticStorage},
    telemetry::HermeticTelemetrySink,
};

/// Shared configuration for stackful object gateway service instances.
#[derive(Clone, Debug)]
pub struct StackfulGatewayConfig {
    pub size_limits: ObjectSizeLimits,
    pub namespace_mapping: NamespaceMapping,
    pub version: String,
    pub request_timeout: Option<Duration>,
}

impl Default for StackfulGatewayConfig {
    fn default() -> Self {
        Self {
            size_limits: ObjectSizeLimits::new(4 * 1024 * 1024, 64 * 1024),
            namespace_mapping: NamespaceMapping::HermeticContainerPerNamespace {
                account: "devstore".to_owned(),
            },
            version: "object-gateway-example".to_owned(),
            request_timeout: None,
        }
    }
}

/// Mutable service state shared by gRPC and HTTP admin handlers.
#[derive(Debug)]
pub struct StackfulGatewayState {
    storage: Mutex<HermeticStorage>,
    telemetry: Mutex<HermeticTelemetrySink>,
    admin: Mutex<AdminStatus>,
    size_limits: ObjectSizeLimits,
    request_timeout: Option<Duration>,
}

impl StackfulGatewayState {
    pub fn new(config: StackfulGatewayConfig) -> Arc<Self> {
        Arc::new(Self {
            storage: Mutex::new(HermeticStorage::new(
                config.size_limits,
                config.namespace_mapping,
            )),
            telemetry: Mutex::new(HermeticTelemetrySink::new()),
            admin: Mutex::new(AdminStatus::ready(config.version)),
            size_limits: config.size_limits,
            request_timeout: config.request_timeout,
        })
    }

    pub fn telemetry(&self) -> HermeticTelemetrySink {
        self.telemetry_mut().clone()
    }

    pub fn admin_status(&self) -> AdminStatus {
        self.admin_mut().clone()
    }

    pub fn begin_shutdown(&self) {
        self.admin_mut().begin_shutdown();
    }

    pub fn inject_storage_failure(&self, failure: super::model::StorageFailureClass) {
        self.storage_mut().inject_failure(failure);
    }

    pub fn fail_next_telemetry_export(&self, message: impl Into<String>) {
        self.telemetry_mut().fail_next_export(message);
    }

    pub fn request_timeout(&self) -> Option<Duration> {
        self.request_timeout
    }

    fn storage_mut(&self) -> MutexGuard<'_, HermeticStorage> {
        self.storage
            .lock()
            .expect("object gateway storage mutex poisoned")
    }

    fn telemetry_mut(&self) -> MutexGuard<'_, HermeticTelemetrySink> {
        self.telemetry
            .lock()
            .expect("object gateway telemetry mutex poisoned")
    }

    fn admin_mut(&self) -> MutexGuard<'_, AdminStatus> {
        self.admin
            .lock()
            .expect("object gateway admin mutex poisoned")
    }
}

/// Registers all canonical object gateway gRPC methods on a server.
pub fn register_grpc_handlers<R>(
    server: &mut RuntimeUnaryServer<R>,
    state: Arc<StackfulGatewayState>,
) where
    R: GrpcRuntime + 'static,
{
    let put_state = state.clone();
    server.add_client_streaming::<proto::PutChunk, proto::PutResponse, _>(
        proto::PUT_PATH,
        move |cx, _metadata, request| handle_put::<R>(cx, put_state.clone(), request),
    );

    let get_state = state.clone();
    server.add_server_streaming::<proto::GetRequest, proto::GetChunk, _, _>(
        proto::GET_PATH,
        move |_cx, _metadata, request| handle_get(get_state.clone(), request),
    );

    let delete_state = state.clone();
    server.add_unary::<proto::DeleteRequest, proto::DeleteResponse, _>(
        proto::DELETE_PATH,
        move |_cx, _metadata, request| Ok(handle_delete(delete_state.clone(), request)),
    );

    let list_state = state.clone();
    server.add_unary::<proto::ListRequest, proto::ListResponse, _>(
        proto::LIST_PATH,
        move |_cx, _metadata, request| Ok(handle_list(list_state.clone(), request)),
    );

    let copy_state = state.clone();
    server.add_unary::<proto::CopyRequest, proto::CopyResponse, _>(
        proto::COPY_PATH,
        move |_cx, _metadata, request| Ok(handle_copy(copy_state.clone(), request)),
    );

    let health_state = state.clone();
    server.add_unary::<proto::HealthRequest, proto::HealthResponse, _>(
        proto::HEALTH_PATH,
        move |_cx, _metadata, _request| {
            let mut outcome = OperationOutcome::success(ObjectOperation::Health, ());
            health_state.telemetry_mut().record(&mut outcome);
            Ok(UnaryReply::new(health_state.admin_status().into()))
        },
    );
}

/// Serves one gRPC request on an already-connected HTTP/2 transport.
pub fn serve_grpc_once<'cx, R, S>(
    cx: &'cx R::Context<'cx>,
    transport: RuntimeStackTransport<S>,
    state: Arc<StackfulGatewayState>,
) -> Result<(), kimojio_stack_grpc::Error>
where
    R: GrpcRuntime + 'static,
    R::Context<'cx>: HttpRuntime<S>,
    S: RuntimeSocket,
{
    serve_grpc_requests::<R, S>(cx, transport, state, 1)
}

/// Serves `requests` gRPC requests on one HTTP/2 connection.
pub fn serve_grpc_requests<'cx, R, S>(
    cx: &'cx R::Context<'cx>,
    mut transport: RuntimeStackTransport<S>,
    state: Arc<StackfulGatewayState>,
    requests: usize,
) -> Result<(), kimojio_stack_grpc::Error>
where
    R: GrpcRuntime + 'static,
    R::Context<'cx>: HttpRuntime<S>,
    S: RuntimeSocket,
{
    transport.set_io_timeout(state.request_timeout());
    let mut http = h2::RuntimeServerConnection::new(transport, HttpConfig::default());
    let mut grpc = RuntimeUnaryServer::<R>::new(ServerConfig::default());
    register_grpc_handlers(&mut grpc, state);
    for _ in 0..requests {
        if !grpc.serve_one(cx, &mut http)? {
            break;
        }
    }
    http.shutdown_write_and_close_after_peer(cx)?;
    Ok(())
}

/// Serves one HTTP/1.1 admin health request on an already-connected transport.
pub fn serve_admin_once<S>(
    cx: &impl HttpRuntime<S>,
    mut transport: RuntimeStackTransport<S>,
    state: Arc<StackfulGatewayState>,
) -> Result<(), kimojio_stack_http::Error>
where
    S: RuntimeSocket,
{
    transport.set_io_timeout(state.request_timeout());
    let mut server = http1::RuntimeServerConnection::new(transport, HttpConfig::default());
    server.serve_one(cx, |request| {
        if request.uri().path() == "/health" || request.uri().path() == "/status" {
            Ok(admin_response(state.admin_status()))
        } else {
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .map_err(|_| kimojio_stack_http::Error::Protocol("failed to build response"))
        }
    })?;
    server.close(cx)
}

fn handle_put<'cx, R>(
    cx: &'cx R::Context<'cx>,
    state: Arc<StackfulGatewayState>,
    mut request: ClientStreamingRequest<'_, proto::PutChunk>,
) -> Result<UnaryReply<proto::PutResponse>, Status>
where
    R: GrpcRuntime,
{
    let first = request
        .next(cx)?
        .ok_or_else(|| Status::new(GrpcStatusCode::InvalidArgument, "empty put stream"))?;
    let key = key_from_proto(first.object.as_ref())?;
    let content_type = if first.content_type.is_empty() {
        "application/octet-stream".to_owned()
    } else {
        first.content_type.clone()
    };
    let expected_key = key.clone();
    let mut outcome =
        match read_put_chunks::<R>(cx, state.size_limits, expected_key, first, &mut request)? {
            Ok(body) => {
                let body = body.into_bytes();
                let mut storage = state.storage_mut();
                storage.put_body(key, content_type, body)
            }
            Err(error) => OperationOutcome::failure(ObjectOperation::Put, error),
        };
    state.telemetry_mut().record(&mut outcome);

    Ok(UnaryReply::new(proto::PutResponse {
        status: Some(status_from_result(&outcome.result)),
        metadata: outcome.result.as_ref().ok().map(metadata_to_proto),
    }))
}

struct ValidatedPutBody {
    chunks: Vec<Bytes>,
    total_len: usize,
}

impl ValidatedPutBody {
    fn into_bytes(self) -> Bytes {
        let mut body = Vec::with_capacity(self.total_len);
        for chunk in self.chunks {
            body.extend_from_slice(&chunk);
        }
        debug_assert_eq!(body.len(), self.total_len);
        Bytes::from(body)
    }
}

fn read_put_chunks<'cx, R>(
    cx: &'cx R::Context<'cx>,
    limits: ObjectSizeLimits,
    expected_key: ObjectKey,
    first: proto::PutChunk,
    request: &mut ClientStreamingRequest<'_, proto::PutChunk>,
) -> Result<Result<ValidatedPutBody, ObjectErrorClass>, Status>
where
    R: GrpcRuntime,
{
    let mut chunks = Vec::new();
    let mut total_len = 0usize;
    let mut next_offset = 0u64;
    let mut finished = false;
    let mut failure = None;
    let mut chunk = Some(first);
    while let Some(current) = chunk {
        if finished {
            return Err(Status::new(
                GrpcStatusCode::InvalidArgument,
                "put chunk arrived after finish",
            ));
        }
        let chunk_key = key_from_proto(current.object.as_ref())?;
        if chunk_key != expected_key {
            return Err(Status::new(
                GrpcStatusCode::InvalidArgument,
                "put chunks changed object identity",
            ));
        }
        if current.offset != next_offset {
            return Err(Status::new(
                GrpcStatusCode::InvalidArgument,
                "put chunk offset was not contiguous",
            ));
        }
        let chunk_len = current.data.len();
        let next_len = next_offset.saturating_add(chunk_len as u64);
        if failure.is_none() {
            if let Err(error) = limits
                .classify_chunk(chunk_len)
                .and_then(|()| limits.classify_object(next_len))
            {
                failure = Some(error);
            } else {
                total_len = total_len.saturating_add(chunk_len);
                chunks.push(current.data);
            }
        }
        next_offset = next_len;
        finished = current.finish;
        chunk = request.next(cx)?;
    }
    Ok(match failure {
        Some(error) => Err(error),
        None => Ok(ValidatedPutBody { chunks, total_len }),
    })
}

fn handle_get(
    state: Arc<StackfulGatewayState>,
    request: proto::GetRequest,
) -> Result<ServerStreamingReply<GatewayGetStream>, Status> {
    let key = key_from_proto(request.object.as_ref())?;
    let max_chunk_bytes = if request.max_chunk_bytes == 0 {
        state.size_limits.max_chunk_bytes
    } else {
        request.max_chunk_bytes as usize
    };
    let mut outcome = if max_chunk_bytes == 0 || max_chunk_bytes > state.size_limits.max_chunk_bytes
    {
        OperationOutcome::failure(ObjectOperation::Get, ObjectErrorClass::SizeLimit)
    } else {
        let mut storage = state.storage_mut();
        storage.get_object(&key)
    };
    state.telemetry_mut().record(&mut outcome);
    let status = status_from_result(&outcome.result);
    Ok(ServerStreamingReply::new(match outcome.result {
        Ok(snapshot) => GatewayGetStream::success(status, snapshot, max_chunk_bytes),
        Err(_) => GatewayGetStream::terminal(status),
    }))
}

pub struct GatewayGetStream {
    status: proto::OperationStatus,
    metadata: Option<proto::ObjectMetadata>,
    data: Bytes,
    offset: usize,
    max_chunk_bytes: usize,
    done: bool,
}

impl GatewayGetStream {
    fn success(
        status: proto::OperationStatus,
        snapshot: super::storage::ObjectSnapshot,
        max_chunk_bytes: usize,
    ) -> Self {
        Self {
            status,
            metadata: Some(metadata_to_proto(&snapshot.metadata)),
            data: snapshot.data,
            offset: 0,
            max_chunk_bytes,
            done: false,
        }
    }

    fn terminal(status: proto::OperationStatus) -> Self {
        Self {
            status,
            metadata: None,
            data: Bytes::new(),
            offset: 0,
            max_chunk_bytes: 1,
            done: false,
        }
    }
}

impl<R> ServerStream<proto::GetChunk, R> for GatewayGetStream
where
    R: GrpcRuntime,
{
    fn next<'cx>(&mut self, _cx: &'cx R::Context<'cx>) -> Result<Option<proto::GetChunk>, Status> {
        if self.done {
            return Ok(None);
        }
        if self.data.is_empty() {
            self.done = true;
            return Ok(Some(proto::GetChunk {
                status: Some(self.status.clone()),
                metadata: self.metadata.take(),
                offset: 0,
                data: Bytes::new(),
                finish: true,
            }));
        }

        let start = self.offset;
        let end = start
            .saturating_add(self.max_chunk_bytes)
            .min(self.data.len());
        self.offset = end;
        let finish = end == self.data.len();
        self.done = finish;
        Ok(Some(proto::GetChunk {
            status: Some(self.status.clone()),
            metadata: self.metadata.take(),
            offset: start as u64,
            data: self.data.slice(start..end),
            finish,
        }))
    }
}

fn handle_delete(
    state: Arc<StackfulGatewayState>,
    request: proto::DeleteRequest,
) -> UnaryReply<proto::DeleteResponse> {
    let result = key_from_proto(request.object.as_ref()).map(|key| {
        let mut outcome = {
            let mut storage = state.storage_mut();
            storage.delete(&key)
        };
        state.telemetry_mut().record(&mut outcome);
        outcome
    });
    let status = match result {
        Ok(outcome) => status_from_result(&outcome.result),
        Err(status) => proto_status(ObjectErrorClass::Internal, status.message()),
    };
    UnaryReply::new(proto::DeleteResponse {
        status: Some(status),
    })
}

fn handle_list(
    state: Arc<StackfulGatewayState>,
    request: proto::ListRequest,
) -> UnaryReply<proto::ListResponse> {
    let mut outcome = {
        let mut storage = state.storage_mut();
        storage.list(&request.namespace, &request.prefix)
    };
    state.telemetry_mut().record(&mut outcome);
    let objects = outcome
        .result
        .as_ref()
        .map(|objects| {
            objects
                .iter()
                .take(list_limit(request.max_results))
                .map(metadata_to_proto)
                .collect()
        })
        .unwrap_or_default();
    UnaryReply::new(proto::ListResponse {
        status: Some(status_from_result(&outcome.result)),
        objects,
    })
}

fn list_limit(max_results: u32) -> usize {
    if max_results == 0 {
        usize::MAX
    } else {
        max_results as usize
    }
}

fn handle_copy(
    state: Arc<StackfulGatewayState>,
    request: proto::CopyRequest,
) -> UnaryReply<proto::CopyResponse> {
    let result = key_from_proto(request.source.as_ref()).and_then(|source| {
        key_from_proto(request.destination.as_ref()).map(|destination| {
            let mut outcome = {
                let mut storage = state.storage_mut();
                storage.copy(&source, destination)
            };
            state.telemetry_mut().record(&mut outcome);
            outcome
        })
    });
    let (status, metadata) = match result {
        Ok(outcome) => (
            status_from_result(&outcome.result),
            outcome.result.as_ref().ok().map(metadata_to_proto),
        ),
        Err(status) => (
            proto_status(ObjectErrorClass::Internal, status.message()),
            None,
        ),
    };
    UnaryReply::new(proto::CopyResponse {
        status: Some(status),
        metadata,
    })
}

fn admin_response(status: AdminStatus) -> Response<Body> {
    let body = format!(
        "ready={} shutting_down={} in_flight={} completed={} failed={} version={}\n",
        status.ready,
        status.shutting_down,
        status.in_flight,
        status.completed,
        status.failed,
        status.version
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/plain")
        .body(Body::copy_from_slice(body.as_bytes(), BodyLimits::new(1024)).unwrap())
        .unwrap()
}

fn key_from_proto(identity: Option<&proto::ObjectIdentity>) -> Result<ObjectKey, Status> {
    let identity =
        identity.ok_or_else(|| Status::new(GrpcStatusCode::InvalidArgument, "missing object"))?;
    if identity.namespace.is_empty() || identity.name.is_empty() {
        return Err(Status::new(
            GrpcStatusCode::InvalidArgument,
            "object namespace and name are required",
        ));
    }
    Ok(ObjectKey::new(&identity.namespace, &identity.name))
}

fn metadata_to_proto(metadata: &ObjectMetadata) -> proto::ObjectMetadata {
    proto::ObjectMetadata {
        object: Some(proto::ObjectIdentity {
            namespace: metadata.key.namespace.as_str().to_owned(),
            name: metadata.key.object.as_str().to_owned(),
        }),
        size_bytes: metadata.size_bytes,
        generation: metadata.generation,
        etag: metadata.etag.clone(),
        content_type: metadata.content_type.clone(),
        user_metadata: metadata
            .user_metadata
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .collect(),
    }
}

fn status_from_result<T>(result: &Result<T, ObjectErrorClass>) -> proto::OperationStatus {
    match result {
        Ok(_) => proto::OperationStatus {
            code: proto::ObjectStatusCode::Ok as i32,
            message: String::new(),
            storage_class: proto::StorageStatusClass::None as i32,
        },
        Err(error) => proto_status(*error, error_message(*error)),
    }
}

fn proto_status(error: ObjectErrorClass, message: impl Into<String>) -> proto::OperationStatus {
    proto::OperationStatus {
        code: match error {
            ObjectErrorClass::NotFound => proto::ObjectStatusCode::NotFound,
            ObjectErrorClass::SizeLimit => proto::ObjectStatusCode::SizeLimit,
            ObjectErrorClass::StorageTimeout => proto::ObjectStatusCode::StorageTimeout,
            ObjectErrorClass::StorageRetriable => proto::ObjectStatusCode::StorageRetriable,
            ObjectErrorClass::StorageNonRetriable => proto::ObjectStatusCode::StorageNonRetriable,
            ObjectErrorClass::DeadlineExceeded => proto::ObjectStatusCode::DeadlineExceeded,
            ObjectErrorClass::Cancelled => proto::ObjectStatusCode::Cancelled,
            ObjectErrorClass::Internal => proto::ObjectStatusCode::Internal,
        } as i32,
        message: message.into(),
        storage_class: match error {
            ObjectErrorClass::StorageTimeout => proto::StorageStatusClass::Timeout,
            ObjectErrorClass::StorageRetriable => proto::StorageStatusClass::Retriable,
            ObjectErrorClass::StorageNonRetriable => proto::StorageStatusClass::NonRetriable,
            _ => proto::StorageStatusClass::None,
        } as i32,
    }
}

fn error_message(error: ObjectErrorClass) -> &'static str {
    match error {
        ObjectErrorClass::NotFound => "object not found",
        ObjectErrorClass::SizeLimit => "object size limit exceeded",
        ObjectErrorClass::StorageTimeout => "storage timeout",
        ObjectErrorClass::StorageRetriable => "storage retriable failure",
        ObjectErrorClass::StorageNonRetriable => "storage non-retriable failure",
        ObjectErrorClass::DeadlineExceeded => "deadline exceeded",
        ObjectErrorClass::Cancelled => "cancelled",
        ObjectErrorClass::Internal => "internal error",
    }
}

/// Small stackful client wrapper used by smoke tests and example binaries.
pub struct StackfulGatewayClient<S> {
    inner: RuntimeUnaryClient<S>,
}

impl<S> StackfulGatewayClient<S> {
    pub fn new(inner: RuntimeUnaryClient<S>) -> Self {
        Self { inner }
    }

    pub fn put(
        &mut self,
        cx: &impl HttpRuntime<S>,
        namespace: &str,
        name: &str,
        chunks: Vec<Bytes>,
    ) -> Result<UnaryResponse<proto::PutResponse>, kimojio_stack_grpc::Error>
    where
        S: RuntimeSocket,
    {
        let chunk_count = chunks.len();
        let requests = chunks
            .into_iter()
            .enumerate()
            .scan(0u64, |offset, (index, data)| {
                let chunk = proto::PutChunk {
                    object: Some(proto::ObjectIdentity {
                        namespace: namespace.to_owned(),
                        name: name.to_owned(),
                    }),
                    offset: *offset,
                    data,
                    finish: index + 1 == chunk_count,
                    content_type: "application/octet-stream".to_owned(),
                    user_metadata: BTreeMap::new().into_iter().collect(),
                };
                *offset += chunk.data.len() as u64;
                Some(chunk)
            });
        self.inner
            .call_client_streaming(cx, proto::PUT_PATH, Metadata::new(), requests)
    }

    pub fn get(
        &mut self,
        cx: &impl HttpRuntime<S>,
        namespace: &str,
        name: &str,
        max_chunk_bytes: u32,
    ) -> Result<Vec<proto::GetChunk>, kimojio_stack_grpc::Error>
    where
        S: RuntimeSocket,
    {
        let mut stream = self.inner.call_server_streaming::<_, proto::GetChunk>(
            cx,
            proto::GET_PATH,
            Metadata::new(),
            &proto::GetRequest {
                object: Some(proto::ObjectIdentity {
                    namespace: namespace.to_owned(),
                    name: name.to_owned(),
                }),
                max_chunk_bytes,
            },
        )?;
        let mut chunks = Vec::new();
        while let Some(chunk) = stream.next(cx)? {
            chunks.push(chunk);
        }
        Ok(chunks)
    }

    pub fn unary<Req, Resp>(
        &mut self,
        cx: &impl HttpRuntime<S>,
        path: &str,
        request: &Req,
    ) -> Result<UnaryResponse<Resp>, kimojio_stack_grpc::Error>
    where
        S: RuntimeSocket,
        Req: prost::Message,
        Resp: prost::Message + Default,
    {
        self.inner.call(cx, path, Metadata::new(), request)
    }

    pub fn close(self, cx: &impl HttpRuntime<S>) -> Result<(), kimojio_stack_grpc::Error>
    where
        S: RuntimeSocket,
    {
        self.inner.close(cx)
    }
}

#[cfg(test)]
mod tests {
    use http::Request;
    use kimojio_stack::{Errno, Runtime};
    use kimojio_stack_http::StackTransport;
    use kimojio_stack_steal::{
        RingFd, Runtime as StealRuntime, RuntimeConfig as StealRuntimeConfig,
    };
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

    use super::*;
    use crate::object_gateway::model::ObjectOperation;

    fn transport_pair() -> (StackTransport, StackTransport) {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();
        (
            StackTransport::plaintext(client_fd),
            StackTransport::plaintext(server_fd),
        )
    }

    #[test]
    fn object_gateway_stack_grpc_smoke_covers_core_operations_and_telemetry() {
        let (client_transport, server_transport) = transport_pair();
        let state = StackfulGatewayState::new(StackfulGatewayConfig {
            size_limits: ObjectSizeLimits::new(1024, 4),
            ..StackfulGatewayConfig::default()
        });
        let server_state = state.clone();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    serve_grpc_requests::<super::super::stack_steal::StackGrpcRuntime, _>(
                        cx,
                        server_transport,
                        server_state,
                        7,
                    )
                    .unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = StackfulGatewayClient::new(RuntimeUnaryClient::new(
                        http,
                        kimojio_stack_grpc::ClientConfig::default(),
                    ));

                    let put = client
                        .put(
                            cx,
                            "ns",
                            "a",
                            vec![Bytes::from_static(b"ab"), Bytes::from_static(b"cd")],
                        )
                        .unwrap();
                    assert_eq!(
                        put.message.status.unwrap().code,
                        proto::ObjectStatusCode::Ok as i32
                    );

                    let chunks = client.get(cx, "ns", "a", 2).unwrap();
                    assert_eq!(
                        chunks
                            .iter()
                            .flat_map(|chunk| chunk.data.iter().copied())
                            .collect::<Vec<_>>(),
                        b"abcd"
                    );

                    let list: UnaryResponse<proto::ListResponse> = client
                        .unary(
                            cx,
                            proto::LIST_PATH,
                            &proto::ListRequest {
                                namespace: "ns".to_owned(),
                                prefix: String::new(),
                                max_results: 0,
                            },
                        )
                        .unwrap();
                    assert_eq!(list.message.objects.len(), 1);

                    let copy: UnaryResponse<proto::CopyResponse> = client
                        .unary(
                            cx,
                            proto::COPY_PATH,
                            &proto::CopyRequest {
                                source: Some(proto::ObjectIdentity {
                                    namespace: "ns".to_owned(),
                                    name: "a".to_owned(),
                                }),
                                destination: Some(proto::ObjectIdentity {
                                    namespace: "ns".to_owned(),
                                    name: "b".to_owned(),
                                }),
                            },
                        )
                        .unwrap();
                    assert_eq!(
                        copy.message.status.unwrap().code,
                        proto::ObjectStatusCode::Ok as i32
                    );

                    let delete: UnaryResponse<proto::DeleteResponse> = client
                        .unary(
                            cx,
                            proto::DELETE_PATH,
                            &proto::DeleteRequest {
                                object: Some(proto::ObjectIdentity {
                                    namespace: "ns".to_owned(),
                                    name: "a".to_owned(),
                                }),
                            },
                        )
                        .unwrap();
                    assert_eq!(
                        delete.message.status.unwrap().code,
                        proto::ObjectStatusCode::Ok as i32
                    );

                    let missing = client.get(cx, "ns", "missing", 2).unwrap();
                    assert_eq!(
                        missing[0].status.as_ref().unwrap().code,
                        proto::ObjectStatusCode::NotFound as i32
                    );

                    let health: UnaryResponse<proto::HealthResponse> = client
                        .unary(cx, proto::HEALTH_PATH, &proto::HealthRequest {})
                        .unwrap();
                    client.close(cx).unwrap();
                    health.message
                });

                server.join(cx);
                let health = client.join(cx);
                assert!(health.ready);
            });
        });

        let telemetry = state.telemetry();
        assert!(telemetry.has_log_and_metric(ObjectOperation::Put, None));
        assert!(telemetry.has_log_and_metric(ObjectOperation::Get, None));
        assert!(
            telemetry.has_log_and_metric(ObjectOperation::Get, Some(ObjectErrorClass::NotFound))
        );
    }

    #[test]
    fn object_gateway_stack_put_stream_enforces_size_limit_and_allows_follow_up() {
        let (client_transport, server_transport) = transport_pair();
        let state = StackfulGatewayState::new(StackfulGatewayConfig {
            size_limits: ObjectSizeLimits::new(4, 4),
            ..StackfulGatewayConfig::default()
        });
        let server_state = state.clone();
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    serve_grpc_requests::<super::super::stack_steal::StackGrpcRuntime, _>(
                        cx,
                        server_transport,
                        server_state,
                        2,
                    )
                    .unwrap();
                });

                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = StackfulGatewayClient::new(RuntimeUnaryClient::new(
                        http,
                        kimojio_stack_grpc::ClientConfig::default(),
                    ));

                    let put = client
                        .put(
                            cx,
                            "ns",
                            "large",
                            vec![Bytes::from_static(b"1234"), Bytes::from_static(b"5")],
                        )
                        .unwrap();
                    assert_eq!(
                        put.message.status.unwrap().code,
                        proto::ObjectStatusCode::SizeLimit as i32
                    );

                    let health: UnaryResponse<proto::HealthResponse> = client
                        .unary(cx, proto::HEALTH_PATH, &proto::HealthRequest {})
                        .unwrap();
                    client.close(cx).unwrap();
                    health.message
                });

                let health = client.join(cx);
                server.join(cx);
                assert!(health.ready);
            });
        });

        assert!(
            state
                .telemetry()
                .has_log_and_metric(ObjectOperation::Put, Some(ObjectErrorClass::SizeLimit))
        );
    }

    #[test]
    fn object_gateway_stack_request_timeout_is_enforced_before_request_headers() {
        let (client_transport, server_transport) = transport_pair();
        let state = StackfulGatewayState::new(StackfulGatewayConfig {
            request_timeout: Some(Duration::from_millis(1)),
            ..StackfulGatewayConfig::default()
        });
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    serve_grpc_once::<super::super::stack_steal::StackGrpcRuntime, _>(
                        cx,
                        server_transport,
                        state,
                    )
                });
                let client = scope.spawn(move |cx| {
                    cx.sleep(Duration::from_millis(20)).unwrap();
                    drop(client_transport);
                });

                let result = server.join(cx);
                client.join(cx);
                match result {
                    Err(kimojio_stack_grpc::Error::Transport(kimojio_stack_http::Error::Io(
                        Errno::TIME,
                    ))) => {}
                    other => panic!("expected request timeout, got {other:?}"),
                }
            });
        });
    }

    #[test]
    fn object_gateway_stack_steal_grpc_smoke_matches_stackful_handlers() {
        let (client_fd, server_fd) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();
        let state = StackfulGatewayState::new(StackfulGatewayConfig {
            size_limits: ObjectSizeLimits::new(1024, 4),
            ..StackfulGatewayConfig::default()
        });
        let server_state = state.clone();
        let mut runtime = StealRuntime::with_config(StealRuntimeConfig {
            workers: std::num::NonZeroUsize::new(2).unwrap(),
            ..StealRuntimeConfig::default()
        });

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn_stealable(move |cx| {
                    let transport =
                        RuntimeStackTransport::plaintext_socket(RingFd::from_owned(server_fd));
                    serve_grpc_requests::<super::super::stack_steal::StackStealGrpcRuntime, _>(
                        cx,
                        transport,
                        server_state,
                        3,
                    )
                    .unwrap();
                });

                let client = scope.spawn_local(move |cx| {
                    let transport =
                        RuntimeStackTransport::plaintext_socket(RingFd::from_owned(client_fd));
                    let http = h2::RuntimeClientConnection::new(transport, HttpConfig::default());
                    let mut client = StackfulGatewayClient::new(RuntimeUnaryClient::new(
                        http,
                        kimojio_stack_grpc::ClientConfig::default(),
                    ));

                    let put = client
                        .put(cx, "ns", "steal", vec![Bytes::from_static(b"xy")])
                        .unwrap();
                    assert_eq!(
                        put.message.status.unwrap().code,
                        proto::ObjectStatusCode::Ok as i32
                    );

                    let chunks = client.get(cx, "ns", "steal", 1).unwrap();
                    assert_eq!(
                        chunks
                            .iter()
                            .flat_map(|chunk| chunk.data.iter().copied())
                            .collect::<Vec<_>>(),
                        b"xy"
                    );

                    let health: UnaryResponse<proto::HealthResponse> = client
                        .unary(cx, proto::HEALTH_PATH, &proto::HealthRequest {})
                        .unwrap();
                    client.close(cx).unwrap();
                    health.message
                });

                let health = client.join(cx);
                server.join(cx);
                assert!(health.ready);
            });
        });

        let telemetry = state.telemetry();
        assert!(telemetry.has_log_and_metric(ObjectOperation::Put, None));
        assert!(telemetry.has_log_and_metric(ObjectOperation::Get, None));
    }

    #[test]
    fn object_gateway_stack_admin_health_status_over_http1() {
        let (client_transport, server_transport) = transport_pair();
        let state = StackfulGatewayState::new(StackfulGatewayConfig::default());
        let mut runtime = Runtime::new();

        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    serve_admin_once(cx, server_transport, state).unwrap();
                });
                let client = scope.spawn(move |cx| {
                    let mut client = kimojio_stack_http::http1::ClientConnection::new(
                        client_transport,
                        HttpConfig::default(),
                    );
                    let request = Request::builder()
                        .method("GET")
                        .uri("/health")
                        .body(Body::empty())
                        .unwrap();
                    let response = client.send(cx, &request).unwrap();
                    client.close(cx).unwrap();
                    response
                });
                server.join(cx);
                let response = client.join(cx);
                assert_eq!(response.status(), StatusCode::OK);
                assert!(
                    std::str::from_utf8(response.body().as_bytes())
                        .unwrap()
                        .contains("ready=true")
                );
            });
        });
    }

    #[test]
    fn object_gateway_stack_reports_size_limit_and_storage_failures() {
        let state = StackfulGatewayState::new(StackfulGatewayConfig {
            size_limits: ObjectSizeLimits::new(4, 4),
            ..StackfulGatewayConfig::default()
        });
        let mut too_large = {
            let mut storage = state.storage_mut();
            let mut chunks =
                vec![Bytes::from_static(b"1234"), Bytes::from_static(b"5")].into_iter();
            storage
                .put_stream(
                    ObjectKey::new("ns", "large"),
                    "application/octet-stream",
                    || Ok::<_, std::convert::Infallible>(chunks.next()),
                )
                .unwrap()
        };
        state.telemetry_mut().record(&mut too_large);
        assert_eq!(too_large.result, Err(ObjectErrorClass::SizeLimit));

        state.inject_storage_failure(super::super::model::StorageFailureClass::Timeout);
        let mut timeout = {
            let mut storage = state.storage_mut();
            storage.get_object(&ObjectKey::new("ns", "x"))
        };
        state.telemetry_mut().record(&mut timeout);
        assert_eq!(timeout.result, Err(ObjectErrorClass::StorageTimeout));

        let telemetry = state.telemetry();
        assert!(
            telemetry.has_log_and_metric(ObjectOperation::Put, Some(ObjectErrorClass::SizeLimit))
        );
        assert!(
            telemetry
                .has_log_and_metric(ObjectOperation::Get, Some(ObjectErrorClass::StorageTimeout))
        );
    }
}
