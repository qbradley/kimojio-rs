// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{
    collections::VecDeque,
    convert::Infallible,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::Poll,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, ensure};
use axum::{
    Router,
    extract::State,
    http::{StatusCode, header::CONTENT_TYPE},
    response::IntoResponse,
    routing::get as axum_get,
};
use bytes::{BufMut, Bytes, BytesMut};
use futures::stream;
use http::uri::PathAndQuery;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};
use tokio_stream::{Stream, StreamExt, wrappers::TcpListenerStream};
use tonic::{
    Code, Request, Response, Status,
    body::Body as TonicBody,
    codegen::{BoxFuture, Service, http},
    transport::{Channel, Endpoint, Server},
};
use tonic_prost::ProstCodec;

use super::{
    admin::AdminStatus,
    model::{
        NamespaceMapping, ObjectErrorClass, ObjectKey, ObjectMetadata, ObjectOperation,
        ObjectSizeLimits, OperationOutcome, StorageBackendMode, StorageFailureClass,
        TelemetrySinkMode,
    },
    proto,
    storage::{GatewayStorage, HermeticStorage, ObjectSnapshot},
    telemetry::HermeticTelemetrySink,
};

type BoxGetStream = Pin<Box<dyn Stream<Item = Result<proto::GetChunk, Status>> + Send + 'static>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokioStorageClient {
    Hermetic,
    AzureStorageBlobsPinned,
}

impl TokioStorageClient {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Hermetic => "hermetic-storage-fixture",
            Self::AzureStorageBlobsPinned => "azure_storage_blobs=0.21",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokioTelemetryClient {
    Hermetic,
    OpenTelemetrySdkPinned,
}

impl TokioTelemetryClient {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Hermetic => "hermetic-otlp-fixture",
            Self::OpenTelemetrySdkPinned => "opentelemetry_sdk=0.32",
        }
    }
}

#[derive(Clone, Debug)]
pub struct TokioGatewayConfig {
    pub size_limits: ObjectSizeLimits,
    pub namespace_mapping: NamespaceMapping,
    pub version: String,
    pub request_timeout: Option<Duration>,
    pub storage_client: TokioStorageClient,
    pub storage_backend: StorageBackendMode,
    pub telemetry_client: TokioTelemetryClient,
    pub telemetry_sink: TelemetrySinkMode,
}

impl Default for TokioGatewayConfig {
    fn default() -> Self {
        Self {
            size_limits: ObjectSizeLimits::new(4 * 1024 * 1024, 64 * 1024),
            namespace_mapping: NamespaceMapping::HermeticContainerPerNamespace {
                account: "devstore".to_owned(),
            },
            version: "object-gateway-tokio".to_owned(),
            request_timeout: None,
            storage_client: TokioStorageClient::Hermetic,
            storage_backend: StorageBackendMode::Hermetic,
            telemetry_client: TokioTelemetryClient::Hermetic,
            telemetry_sink: TelemetrySinkMode::Hermetic,
        }
    }
}

#[derive(Debug)]
pub struct TokioGatewayState {
    storage: Mutex<HermeticStorage>,
    telemetry: Mutex<HermeticTelemetrySink>,
    admin: Mutex<AdminStatus>,
    delayed_gets: Mutex<VecDeque<Duration>>,
    size_limits: ObjectSizeLimits,
    request_timeout: Option<Duration>,
    config: TokioGatewayConfig,
}

impl TokioGatewayState {
    pub fn new(config: TokioGatewayConfig) -> Arc<Self> {
        Arc::new(Self {
            storage: Mutex::new(HermeticStorage::new(
                config.size_limits,
                config.namespace_mapping.clone(),
            )),
            telemetry: Mutex::new(HermeticTelemetrySink::new()),
            admin: Mutex::new(AdminStatus::ready(config.version.clone())),
            delayed_gets: Mutex::new(VecDeque::new()),
            size_limits: config.size_limits,
            request_timeout: config.request_timeout,
            config,
        })
    }

    pub fn admin_status(&self) -> AdminStatus {
        self.admin_mut().clone()
    }

    pub fn begin_shutdown(&self) {
        self.admin_mut().begin_shutdown();
    }

    pub fn telemetry(&self) -> HermeticTelemetrySink {
        self.telemetry_mut().clone()
    }

    pub fn inject_storage_failure(&self, failure: StorageFailureClass) {
        self.storage_mut().inject_failure(failure);
    }

    pub fn fail_next_telemetry_export(&self, message: impl Into<String>) {
        self.telemetry_mut().fail_next_export(message);
    }

    pub fn delay_next_get(&self, delay: Duration) {
        self.delayed_gets_mut().push_back(delay);
    }

    pub fn request_timeout(&self) -> Option<Duration> {
        self.request_timeout
    }

    pub fn config(&self) -> &TokioGatewayConfig {
        &self.config
    }

    fn storage_mut(&self) -> MutexGuard<'_, HermeticStorage> {
        self.storage
            .lock()
            .expect("object gateway Tokio storage mutex poisoned")
    }

    fn telemetry_mut(&self) -> MutexGuard<'_, HermeticTelemetrySink> {
        self.telemetry
            .lock()
            .expect("object gateway Tokio telemetry mutex poisoned")
    }

    fn admin_mut(&self) -> MutexGuard<'_, AdminStatus> {
        self.admin
            .lock()
            .expect("object gateway Tokio admin mutex poisoned")
    }

    fn delayed_gets_mut(&self) -> MutexGuard<'_, VecDeque<Duration>> {
        self.delayed_gets
            .lock()
            .expect("object gateway Tokio delayed GET mutex poisoned")
    }

    fn take_next_get_delay(&self) -> Option<Duration> {
        self.delayed_gets_mut().pop_front()
    }
}

#[derive(Clone)]
pub struct ObjectGatewayTonicServer {
    state: Arc<TokioGatewayState>,
}

impl ObjectGatewayTonicServer {
    pub fn new(state: Arc<TokioGatewayState>) -> Self {
        Self { state }
    }
}

impl tonic::server::NamedService for ObjectGatewayTonicServer {
    const NAME: &'static str = proto::SERVICE_NAME;
}

impl Service<http::Request<TonicBody>> for ObjectGatewayTonicServer {
    type Response = http::Response<TonicBody>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<TonicBody>) -> Self::Future {
        match request.uri().path() {
            proto::PUT_PATH => {
                let state = self.state.clone();
                Box::pin(async move {
                    let codec = ProstCodec::<proto::PutResponse, proto::PutChunk>::default();
                    let mut grpc = tonic::server::Grpc::new(codec);
                    Ok(grpc.client_streaming(TokioPut { state }, request).await)
                })
            }
            proto::GET_PATH => {
                let state = self.state.clone();
                Box::pin(async move {
                    let codec = ProstCodec::<proto::GetChunk, proto::GetRequest>::default();
                    let mut grpc = tonic::server::Grpc::new(codec);
                    Ok(grpc.server_streaming(TokioGet { state }, request).await)
                })
            }
            proto::DELETE_PATH => {
                let state = self.state.clone();
                Box::pin(async move {
                    let codec =
                        ProstCodec::<proto::DeleteResponse, proto::DeleteRequest>::default();
                    let mut grpc = tonic::server::Grpc::new(codec);
                    Ok(grpc.unary(TokioDelete { state }, request).await)
                })
            }
            proto::LIST_PATH => {
                let state = self.state.clone();
                Box::pin(async move {
                    let codec = ProstCodec::<proto::ListResponse, proto::ListRequest>::default();
                    let mut grpc = tonic::server::Grpc::new(codec);
                    Ok(grpc.unary(TokioList { state }, request).await)
                })
            }
            proto::COPY_PATH => {
                let state = self.state.clone();
                Box::pin(async move {
                    let codec = ProstCodec::<proto::CopyResponse, proto::CopyRequest>::default();
                    let mut grpc = tonic::server::Grpc::new(codec);
                    Ok(grpc.unary(TokioCopy { state }, request).await)
                })
            }
            proto::HEALTH_PATH => {
                let state = self.state.clone();
                Box::pin(async move {
                    let codec =
                        ProstCodec::<proto::HealthResponse, proto::HealthRequest>::default();
                    let mut grpc = tonic::server::Grpc::new(codec);
                    Ok(grpc.unary(TokioHealth { state }, request).await)
                })
            }
            _ => Box::pin(async move { Ok(Status::unimplemented("unknown method").into_http()) }),
        }
    }
}

#[derive(Clone)]
struct TokioPut {
    state: Arc<TokioGatewayState>,
}

impl tonic::server::ClientStreamingService<proto::PutChunk> for TokioPut {
    type Response = proto::PutResponse;
    type Future = BoxFuture<Response<Self::Response>, Status>;

    fn call(&mut self, request: Request<tonic::Streaming<proto::PutChunk>>) -> Self::Future {
        let state = self.state.clone();
        Box::pin(async move {
            handle_put(state, request.into_inner())
                .await
                .map(Response::new)
        })
    }
}

#[derive(Clone)]
struct TokioGet {
    state: Arc<TokioGatewayState>,
}

impl tonic::server::ServerStreamingService<proto::GetRequest> for TokioGet {
    type Response = proto::GetChunk;
    type ResponseStream = BoxGetStream;
    type Future = BoxFuture<Response<Self::ResponseStream>, Status>;

    fn call(&mut self, request: Request<proto::GetRequest>) -> Self::Future {
        let state = self.state.clone();
        Box::pin(async move {
            if let Some(delay) = state.take_next_get_delay() {
                tokio::time::sleep(delay).await;
            }
            handle_get(state, request.into_inner()).map(Response::new)
        })
    }
}

#[derive(Clone)]
struct TokioDelete {
    state: Arc<TokioGatewayState>,
}

impl tonic::server::UnaryService<proto::DeleteRequest> for TokioDelete {
    type Response = proto::DeleteResponse;
    type Future = BoxFuture<Response<Self::Response>, Status>;

    fn call(&mut self, request: Request<proto::DeleteRequest>) -> Self::Future {
        let state = self.state.clone();
        Box::pin(async move { Ok(Response::new(handle_delete(state, request.into_inner()))) })
    }
}

#[derive(Clone)]
struct TokioList {
    state: Arc<TokioGatewayState>,
}

impl tonic::server::UnaryService<proto::ListRequest> for TokioList {
    type Response = proto::ListResponse;
    type Future = BoxFuture<Response<Self::Response>, Status>;

    fn call(&mut self, request: Request<proto::ListRequest>) -> Self::Future {
        let state = self.state.clone();
        Box::pin(async move { Ok(Response::new(handle_list(state, request.into_inner()))) })
    }
}

#[derive(Clone)]
struct TokioCopy {
    state: Arc<TokioGatewayState>,
}

impl tonic::server::UnaryService<proto::CopyRequest> for TokioCopy {
    type Response = proto::CopyResponse;
    type Future = BoxFuture<Response<Self::Response>, Status>;

    fn call(&mut self, request: Request<proto::CopyRequest>) -> Self::Future {
        let state = self.state.clone();
        Box::pin(async move { Ok(Response::new(handle_copy(state, request.into_inner()))) })
    }
}

#[derive(Clone)]
struct TokioHealth {
    state: Arc<TokioGatewayState>,
}

impl tonic::server::UnaryService<proto::HealthRequest> for TokioHealth {
    type Response = proto::HealthResponse;
    type Future = BoxFuture<Response<Self::Response>, Status>;

    fn call(&mut self, _request: Request<proto::HealthRequest>) -> Self::Future {
        let state = self.state.clone();
        Box::pin(async move {
            let mut outcome = OperationOutcome::success(ObjectOperation::Health, ());
            state.telemetry_mut().record(&mut outcome);
            Ok(Response::new(state.admin_status().into()))
        })
    }
}

async fn handle_put(
    state: Arc<TokioGatewayState>,
    mut stream: tonic::Streaming<proto::PutChunk>,
) -> Result<proto::PutResponse, Status> {
    let first = stream
        .message()
        .await?
        .ok_or_else(|| Status::new(Code::InvalidArgument, "empty put stream"))?;
    let key = key_from_proto(first.object.as_ref())?;
    let content_type = if first.content_type.is_empty() {
        "application/octet-stream".to_owned()
    } else {
        first.content_type.clone()
    };
    let mut body = BytesMut::new();
    let mut next_offset = 0u64;
    let mut finished = false;
    if let Err(status) = append_put_chunk(
        &state,
        &key,
        &mut body,
        &first,
        &mut next_offset,
        &mut finished,
    ) {
        let mut outcome = OperationOutcome::<ObjectMetadata>::failure(ObjectOperation::Put, status);
        state.telemetry_mut().record(&mut outcome);
        return Ok(proto::PutResponse {
            status: Some(status_from_result(&outcome.result)),
            metadata: None,
        });
    }
    while let Some(chunk) = stream.message().await? {
        if let Err(status) = append_put_chunk(
            &state,
            &key,
            &mut body,
            &chunk,
            &mut next_offset,
            &mut finished,
        ) {
            while stream.message().await?.is_some() {}
            let mut outcome =
                OperationOutcome::<ObjectMetadata>::failure(ObjectOperation::Put, status);
            state.telemetry_mut().record(&mut outcome);
            return Ok(proto::PutResponse {
                status: Some(status_from_result(&outcome.result)),
                metadata: None,
            });
        }
    }

    let mut outcome = {
        let mut storage = state.storage_mut();
        storage.put_body(key, content_type, body.freeze())
    };
    state.telemetry_mut().record(&mut outcome);
    Ok(proto::PutResponse {
        status: Some(status_from_result(&outcome.result)),
        metadata: outcome.result.as_ref().ok().map(metadata_to_proto),
    })
}

fn append_put_chunk(
    state: &TokioGatewayState,
    expected_key: &ObjectKey,
    body: &mut BytesMut,
    chunk: &proto::PutChunk,
    next_offset: &mut u64,
    finished: &mut bool,
) -> Result<(), ObjectErrorClass> {
    if *finished {
        return Err(ObjectErrorClass::Internal);
    }
    let chunk_key =
        key_from_proto(chunk.object.as_ref()).map_err(|_| ObjectErrorClass::Internal)?;
    if &chunk_key != expected_key {
        return Err(ObjectErrorClass::Internal);
    }
    if chunk.offset != *next_offset {
        return Err(ObjectErrorClass::Internal);
    }
    state.size_limits.classify_chunk(chunk.data.len())?;
    let next_len = body.len().saturating_add(chunk.data.len());
    state.size_limits.classify_object(next_len as u64)?;
    body.put(chunk.data.as_ref());
    *next_offset = (*next_offset).saturating_add(chunk.data.len() as u64);
    *finished = chunk.finish;
    Ok(())
}

fn handle_get(
    state: Arc<TokioGatewayState>,
    request: proto::GetRequest,
) -> Result<BoxGetStream, Status> {
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
    Ok(match outcome.result {
        Ok(snapshot) => get_success_stream(status, snapshot, max_chunk_bytes),
        Err(_) => Box::pin(tokio_stream::iter([Ok(proto::GetChunk {
            status: Some(status),
            metadata: None,
            offset: 0,
            data: Bytes::new(),
            finish: true,
        })])),
    })
}

fn get_success_stream(
    status: proto::OperationStatus,
    snapshot: ObjectSnapshot,
    max_chunk_bytes: usize,
) -> BoxGetStream {
    #[derive(Clone)]
    struct State {
        status: proto::OperationStatus,
        metadata: Option<proto::ObjectMetadata>,
        data: Bytes,
        offset: usize,
        max_chunk_bytes: usize,
        emitted_empty: bool,
    }

    Box::pin(stream::unfold(
        State {
            status,
            metadata: Some(metadata_to_proto(&snapshot.metadata)),
            data: snapshot.data,
            offset: 0,
            max_chunk_bytes,
            emitted_empty: false,
        },
        |mut state| async move {
            if state.data.is_empty() {
                if state.emitted_empty {
                    return None;
                }
                state.emitted_empty = true;
                let chunk = proto::GetChunk {
                    status: Some(state.status.clone()),
                    metadata: state.metadata.take(),
                    offset: 0,
                    data: Bytes::new(),
                    finish: true,
                };
                return Some((Ok(chunk), state));
            }
            if state.offset >= state.data.len() {
                return None;
            }
            let start = state.offset;
            let end = start
                .saturating_add(state.max_chunk_bytes)
                .min(state.data.len());
            state.offset = end;
            let chunk = proto::GetChunk {
                status: Some(state.status.clone()),
                metadata: state.metadata.take(),
                offset: start as u64,
                data: state.data.slice(start..end),
                finish: end == state.data.len(),
            };
            Some((Ok(chunk), state))
        },
    ))
}

fn handle_delete(
    state: Arc<TokioGatewayState>,
    request: proto::DeleteRequest,
) -> proto::DeleteResponse {
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
    proto::DeleteResponse {
        status: Some(status),
    }
}

fn handle_list(state: Arc<TokioGatewayState>, request: proto::ListRequest) -> proto::ListResponse {
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
    proto::ListResponse {
        status: Some(status_from_result(&outcome.result)),
        objects,
    }
}

fn list_limit(max_results: u32) -> usize {
    if max_results == 0 {
        usize::MAX
    } else {
        max_results as usize
    }
}

fn handle_copy(state: Arc<TokioGatewayState>, request: proto::CopyRequest) -> proto::CopyResponse {
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
    proto::CopyResponse {
        status: Some(status),
        metadata,
    }
}

pub async fn serve_grpc_listener(
    listener: TcpListener,
    state: Arc<TokioGatewayState>,
    shutdown: oneshot::Receiver<()>,
) -> Result<()> {
    let mut builder = Server::builder();
    if let Some(timeout) = state.request_timeout() {
        builder = builder.timeout(timeout);
    }
    let incoming = TcpListenerStream::new(listener).map(|stream| {
        let stream = stream?;
        stream.set_nodelay(true)?;
        Ok::<_, std::io::Error>(stream)
    });
    builder
        .add_service(ObjectGatewayTonicServer::new(state))
        .serve_with_incoming_shutdown(incoming, async {
            let _ = shutdown.await;
        })
        .await
        .context("Tokio gRPC server failed")
}

pub async fn serve_admin_listener(
    listener: TcpListener,
    state: Arc<TokioGatewayState>,
    shutdown: oneshot::Receiver<()>,
) -> Result<()> {
    let app = Router::new()
        .route("/health", axum_get(admin_handler))
        .route("/status", axum_get(admin_handler))
        .with_state(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = shutdown.await;
        })
        .await
        .context("Tokio Axum admin server failed")
}

async fn admin_handler(State(state): State<Arc<TokioGatewayState>>) -> impl IntoResponse {
    let status = state.admin_status();
    let body = format!(
        "ready={} shutting_down={} in_flight={} completed={} failed={} version={}\n",
        status.ready,
        status.shutting_down,
        status.in_flight,
        status.completed,
        status.failed,
        status.version
    );
    (StatusCode::OK, [(CONTENT_TYPE, "text/plain")], body)
}

pub async fn tokio_grpc_health(addr: SocketAddr) -> Result<proto::HealthResponse> {
    let channel = connect_channel(addr).await?;
    tonic_unary::<proto::HealthRequest, proto::HealthResponse>(
        channel,
        proto::HEALTH_PATH,
        proto::HealthRequest {},
    )
    .await
}

pub async fn tokio_admin_get(addr: SocketAddr, path: &str) -> Result<String> {
    let mut stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("failed to connect to admin target {addr}"))?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .context("failed to write admin request")?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .context("failed to read admin response")?;
    let response = String::from_utf8(response).context("admin response was not UTF-8")?;
    ensure!(
        response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200"),
        "admin request {path} failed: {response}"
    );
    Ok(response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default())
}

#[derive(Clone, Debug)]
pub struct TokioGateway {
    state: Arc<TokioGatewayState>,
    worker_threads: usize,
}

impl TokioGateway {
    pub fn new(config: TokioGatewayConfig) -> Self {
        Self {
            state: TokioGatewayState::new(config),
            worker_threads: 2,
        }
    }

    pub fn state(&self) -> Arc<TokioGatewayState> {
        self.state.clone()
    }

    pub fn inject_storage_failure(&self, failure: StorageFailureClass) {
        self.state.inject_storage_failure(failure);
    }

    pub fn fail_next_telemetry_export(&self, message: impl Into<String>) {
        self.state.fail_next_telemetry_export(message);
    }

    pub fn telemetry(&self) -> HermeticTelemetrySink {
        self.state.telemetry()
    }

    pub fn put(
        &self,
        namespace: &str,
        object: &str,
        chunks: Vec<Bytes>,
    ) -> Result<proto::PutResponse> {
        let namespace = namespace.to_owned();
        let object = object.to_owned();
        self.block_on_with_grpc_server(move |addr| async move {
            let channel = connect_channel(addr).await?;
            tokio_put(channel, &namespace, &object, chunks).await
        })
    }

    pub fn get(
        &self,
        namespace: &str,
        object: &str,
        max_chunk_bytes: u32,
    ) -> Result<Vec<proto::GetChunk>> {
        let namespace = namespace.to_owned();
        let object = object.to_owned();
        self.block_on_with_grpc_server(move |addr| async move {
            let channel = connect_channel(addr).await?;
            tokio_get(channel, &namespace, &object, max_chunk_bytes).await
        })
    }

    pub fn delete(&self, namespace: &str, object: &str) -> Result<proto::DeleteResponse> {
        let request = proto::DeleteRequest {
            object: Some(identity(namespace, object)),
        };
        self.block_on_with_grpc_server(move |addr| async move {
            tonic_unary(connect_channel(addr).await?, proto::DELETE_PATH, request).await
        })
    }

    pub fn list(&self, namespace: &str, prefix: &str) -> Result<proto::ListResponse> {
        let request = proto::ListRequest {
            namespace: namespace.to_owned(),
            prefix: prefix.to_owned(),
            max_results: 0,
        };
        self.block_on_with_grpc_server(move |addr| async move {
            tonic_unary(connect_channel(addr).await?, proto::LIST_PATH, request).await
        })
    }

    pub fn copy(
        &self,
        namespace: &str,
        source: &str,
        destination: &str,
    ) -> Result<proto::CopyResponse> {
        let request = proto::CopyRequest {
            source: Some(identity(namespace, source)),
            destination: Some(identity(namespace, destination)),
        };
        self.block_on_with_grpc_server(move |addr| async move {
            tonic_unary(connect_channel(addr).await?, proto::COPY_PATH, request).await
        })
    }

    pub fn grpc_health(&self) -> Result<proto::HealthResponse> {
        self.block_on_with_grpc_server(move |addr| async move { tokio_grpc_health(addr).await })
    }

    pub fn admin_health(&self) -> Result<String> {
        self.block_on_with_admin_server(move |addr| async move {
            tokio_admin_get(addr, "/health").await
        })
    }

    pub fn admin_status(&self) -> Result<String> {
        self.block_on_with_admin_server(move |addr| async move {
            tokio_admin_get(addr, "/status").await
        })
    }

    pub fn cancel_get_after_first_chunk(&self, namespace: &str, object: &str) -> Result<()> {
        let namespace = namespace.to_owned();
        let object = object.to_owned();
        self.block_on_with_grpc_server(move |addr| async move {
            let channel = connect_channel(addr).await?;
            let mut grpc = tonic::client::Grpc::new(channel);
            grpc.ready()
                .await
                .map_err(|error| Status::unavailable(error.to_string()))?;
            let response = grpc
                .server_streaming(
                    Request::new(proto::GetRequest {
                        object: Some(identity(&namespace, &object)),
                        max_chunk_bytes: 1,
                    }),
                    PathAndQuery::from_static(proto::GET_PATH),
                    ProstCodec::<proto::GetRequest, proto::GetChunk>::default(),
                )
                .await?;
            let mut stream = response.into_inner();
            ensure!(
                stream.message().await?.is_some(),
                "GET stream ended before cancellation"
            );
            drop(stream);
            Ok(())
        })
    }

    pub fn idle_grpc_request_times_out(&self) -> Result<()> {
        let timeout = self
            .state
            .request_timeout()
            .context("Tokio deadline scenario requires request timeout")?;
        self.state.delay_next_get(timeout.saturating_mul(2));
        self.block_on_with_grpc_server(move |addr| async move {
            let channel = connect_channel(addr).await?;
            let mut grpc = tonic::client::Grpc::new(channel);
            grpc.ready()
                .await
                .map_err(|error| Status::unavailable(error.to_string()))?;
            let status = grpc
                .server_streaming(
                    Request::new(proto::GetRequest {
                        object: Some(identity("deadline", "slow")),
                        max_chunk_bytes: 4,
                    }),
                    PathAndQuery::from_static(proto::GET_PATH),
                    ProstCodec::<proto::GetRequest, proto::GetChunk>::default(),
                )
                .await
                .expect_err("delayed GET should exceed the configured tonic server timeout");
            ensure!(
                matches!(status.code(), Code::DeadlineExceeded | Code::Cancelled),
                "expected tonic timeout/cancellation status, got {status:?}"
            );
            Ok(())
        })
    }

    pub fn put_pair_concurrently(
        &self,
        namespace: &str,
        first: (&str, Bytes),
        second: (&str, Bytes),
    ) -> Result<(proto::PutResponse, proto::PutResponse)> {
        std::thread::scope(|scope| {
            let first_gateway = self.clone();
            let second_gateway = self.clone();
            let namespace = namespace.to_owned();
            let first = (first.0.to_owned(), first.1);
            let second = (second.0.to_owned(), second.1);
            let first_namespace = namespace.clone();
            let second_namespace = namespace;
            let first_handle =
                scope.spawn(move || first_gateway.put(&first_namespace, &first.0, vec![first.1]));
            let second_handle = scope
                .spawn(move || second_gateway.put(&second_namespace, &second.0, vec![second.1]));
            let first = first_handle
                .join()
                .map_err(|_| anyhow!("first concurrent Tokio PUT panicked"))??;
            let second = second_handle
                .join()
                .map_err(|_| anyhow!("second concurrent Tokio PUT panicked"))??;
            Ok((first, second))
        })
    }

    fn block_on_with_grpc_server<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(SocketAddr) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let state = self.state.clone();
        self.runtime()?.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .context("failed to bind local Tokio gRPC listener")?;
            let addr = listener.local_addr().context("failed to read gRPC addr")?;
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            let server = tokio::spawn(serve_grpc_listener(listener, state, shutdown_rx));
            let result = f(addr).await;
            let _ = shutdown_tx.send(());
            server.await.context("Tokio gRPC server task panicked")??;
            result
        })
    }

    fn block_on_with_admin_server<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(SocketAddr) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let state = self.state.clone();
        self.runtime()?.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .context("failed to bind local Tokio admin listener")?;
            let addr = listener.local_addr().context("failed to read admin addr")?;
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            let server = tokio::spawn(serve_admin_listener(listener, state, shutdown_rx));
            let result = f(addr).await;
            let _ = shutdown_tx.send(());
            server.await.context("Tokio admin server task panicked")??;
            result
        })
    }

    fn runtime(&self) -> Result<tokio::runtime::Runtime> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(self.worker_threads)
            .enable_all()
            .build()
            .context("failed to create Tokio comparison runtime")
    }
}

async fn connect_channel(addr: SocketAddr) -> Result<Channel> {
    Endpoint::from_shared(format!("http://{addr}"))?
        .connect()
        .await
        .with_context(|| format!("failed to connect to Tokio gRPC target {addr}"))
}

async fn tonic_unary<Req, Resp>(channel: Channel, path: &'static str, request: Req) -> Result<Resp>
where
    Req: prost::Message + Default + Send + 'static,
    Resp: prost::Message + Default + Send + 'static,
{
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready()
        .await
        .map_err(|error| Status::unavailable(error.to_string()))?;
    let response = grpc
        .unary(
            Request::new(request),
            PathAndQuery::from_static(path),
            ProstCodec::<Req, Resp>::default(),
        )
        .await?;
    Ok(response.into_inner())
}

async fn tokio_put(
    channel: Channel,
    namespace: &str,
    object: &str,
    chunks: Vec<Bytes>,
) -> Result<proto::PutResponse> {
    let chunk_count = chunks.len();
    let requests = chunks
        .into_iter()
        .enumerate()
        .scan(0u64, |offset, (index, data)| {
            let chunk = proto::PutChunk {
                object: Some(identity(namespace, object)),
                offset: *offset,
                data,
                finish: index + 1 == chunk_count,
                content_type: "application/octet-stream".to_owned(),
                user_metadata: Default::default(),
            };
            *offset += chunk.data.len() as u64;
            Some(chunk)
        });
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready()
        .await
        .map_err(|error| Status::unavailable(error.to_string()))?;
    let response = grpc
        .client_streaming(
            Request::new(tokio_stream::iter(requests.collect::<Vec<_>>())),
            PathAndQuery::from_static(proto::PUT_PATH),
            ProstCodec::<proto::PutChunk, proto::PutResponse>::default(),
        )
        .await?;
    Ok(response.into_inner())
}

async fn tokio_get(
    channel: Channel,
    namespace: &str,
    object: &str,
    max_chunk_bytes: u32,
) -> Result<Vec<proto::GetChunk>> {
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready()
        .await
        .map_err(|error| Status::unavailable(error.to_string()))?;
    let response = grpc
        .server_streaming(
            Request::new(proto::GetRequest {
                object: Some(identity(namespace, object)),
                max_chunk_bytes,
            }),
            PathAndQuery::from_static(proto::GET_PATH),
            ProstCodec::<proto::GetRequest, proto::GetChunk>::default(),
        )
        .await?;
    let mut stream = response.into_inner();
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.message().await? {
        chunks.push(chunk);
    }
    Ok(chunks)
}

fn key_from_proto(identity: Option<&proto::ObjectIdentity>) -> Result<ObjectKey, Status> {
    let identity = identity.ok_or_else(|| Status::new(Code::InvalidArgument, "missing object"))?;
    if identity.namespace.is_empty() || identity.name.is_empty() {
        return Err(Status::new(
            Code::InvalidArgument,
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
        user_metadata: metadata.user_metadata.clone().into_iter().collect(),
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

fn identity(namespace: &str, object: &str) -> proto::ObjectIdentity {
    proto::ObjectIdentity {
        namespace: namespace.to_owned(),
        name: object.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> TokioGateway {
        TokioGateway::new(TokioGatewayConfig {
            size_limits: ObjectSizeLimits::new(16, 4),
            namespace_mapping: NamespaceMapping::HermeticContainerPerNamespace {
                account: "devstore".to_owned(),
            },
            version: "tokio-test".to_owned(),
            request_timeout: Some(Duration::from_millis(50)),
            ..TokioGatewayConfig::default()
        })
    }

    #[test]
    fn object_gateway_tokio_grpc_round_trips_put_get_health() {
        let gateway = fixture();
        let put = gateway
            .put(
                "tokio",
                "object",
                vec![Bytes::from_static(b"ab"), Bytes::from_static(b"cd")],
            )
            .unwrap();
        assert_eq!(put.status.unwrap().code, proto::ObjectStatusCode::Ok as i32);
        let chunks = gateway.get("tokio", "object", 2).unwrap();
        assert_eq!(
            chunks
                .iter()
                .flat_map(|chunk| chunk.data.iter().copied())
                .collect::<Vec<_>>(),
            b"abcd"
        );
        assert!(gateway.grpc_health().unwrap().ready);
    }

    #[test]
    fn object_gateway_tokio_axum_admin_reports_ready() {
        let gateway = fixture();
        assert!(gateway.admin_health().unwrap().contains("ready=true"));
        assert!(
            gateway
                .admin_status()
                .unwrap()
                .contains("version=tokio-test")
        );
    }

    #[test]
    fn object_gateway_tokio_storage_and_telemetry_failures_are_classified() {
        let gateway = fixture();
        gateway.inject_storage_failure(StorageFailureClass::Timeout);
        let chunks = gateway.get("tokio", "missing", 4).unwrap();
        assert_eq!(
            chunks[0].status.as_ref().unwrap().code,
            proto::ObjectStatusCode::StorageTimeout as i32
        );
        assert!(
            gateway
                .telemetry()
                .has_log_and_metric(ObjectOperation::Get, Some(ObjectErrorClass::StorageTimeout))
        );

        gateway.fail_next_telemetry_export("collector unavailable");
        let put = gateway
            .put("tokio", "ok", vec![Bytes::from_static(b"ok")])
            .unwrap();
        assert_eq!(put.status.unwrap().code, proto::ObjectStatusCode::Ok as i32);
        assert!(!gateway.telemetry().diagnostics().is_empty());
    }
}
