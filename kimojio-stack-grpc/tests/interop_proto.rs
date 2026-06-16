#![allow(dead_code)]

use std::{convert::Infallible, pin::Pin, sync::Arc, task::Poll};

use http::uri::PathAndQuery;
use prost::Message;
use tonic::{
    Code, Request, Response, Status,
    body::Body as TonicBody,
    codegen::{BoxFuture, Service, http, tokio_stream::Stream},
    metadata::{MetadataValue, errors::InvalidMetadataValue},
    transport::Channel,
};
use tonic_prost::ProstCodec;

pub const SERVICE_NAME: &str = "interop.TestService";
pub const UNARY_PATH: &str = "/interop.TestService/Unary";
pub const STREAM_PATH: &str = "/interop.TestService/Stream";
pub const CLIENT_STREAM_PATH: &str = "/interop.TestService/ClientStream";

#[derive(Clone, PartialEq, Message)]
pub struct TestRequest {
    #[prost(string, tag = "1")]
    pub value: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct TestResponse {
    #[prost(string, tag = "1")]
    pub value: String,
}

#[derive(Clone)]
pub enum TonicBehavior {
    Success,
    Error,
}

#[derive(Clone)]
pub struct TonicTestServer {
    behavior: Arc<TonicBehavior>,
}

impl TonicTestServer {
    pub fn new(behavior: TonicBehavior) -> Self {
        Self {
            behavior: Arc::new(behavior),
        }
    }
}

impl tonic::server::NamedService for TonicTestServer {
    const NAME: &'static str = SERVICE_NAME;
}

impl Service<http::Request<TonicBody>> for TonicTestServer {
    type Response = http::Response<TonicBody>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<TonicBody>) -> Self::Future {
        match request.uri().path() {
            UNARY_PATH => {
                let behavior = self.behavior.clone();
                Box::pin(async move {
                    let codec = ProstCodec::<TestResponse, TestRequest>::default();
                    let mut grpc = tonic::server::Grpc::new(codec);
                    Ok(grpc.unary(TonicUnary { behavior }, request).await)
                })
            }
            STREAM_PATH => {
                let behavior = self.behavior.clone();
                Box::pin(async move {
                    let codec = ProstCodec::<TestResponse, TestRequest>::default();
                    let mut grpc = tonic::server::Grpc::new(codec);
                    Ok(grpc
                        .server_streaming(TonicServerStreaming { behavior }, request)
                        .await)
                })
            }
            CLIENT_STREAM_PATH => {
                let behavior = self.behavior.clone();
                Box::pin(async move {
                    let codec = ProstCodec::<TestResponse, TestRequest>::default();
                    let mut grpc = tonic::server::Grpc::new(codec);
                    Ok(grpc
                        .client_streaming(TonicClientStreaming { behavior }, request)
                        .await)
                })
            }
            _ => Box::pin(async move { Ok(Status::unimplemented("unknown method").into_http()) }),
        }
    }
}

#[derive(Clone)]
struct TonicUnary {
    behavior: Arc<TonicBehavior>,
}

impl tonic::server::UnaryService<TestRequest> for TonicUnary {
    type Response = TestResponse;
    type Future = BoxFuture<Response<Self::Response>, Status>;

    fn call(&mut self, request: Request<TestRequest>) -> Self::Future {
        let behavior = self.behavior.clone();
        Box::pin(async move {
            assert_eq!(request.metadata().get("x-request").unwrap(), "stackful");
            assert_eq!(
                request
                    .metadata()
                    .get_bin("trace-bin")
                    .unwrap()
                    .to_bytes()
                    .unwrap()
                    .as_ref(),
                b"trace"
            );

            match &*behavior {
                TonicBehavior::Success => {
                    let mut response = Response::new(TestResponse {
                        value: format!("tonic {}", request.get_ref().value),
                    });
                    response
                        .metadata_mut()
                        .insert("x-tonic", MetadataValue::from_static("yes"));
                    response
                        .metadata_mut()
                        .insert_bin("trace-bin", MetadataValue::from_bytes(b"tonic-response"));
                    Ok(response)
                }
                TonicBehavior::Error => {
                    let mut metadata = tonic::metadata::MetadataMap::new();
                    metadata.insert("x-error", MetadataValue::from_static("tonic"));
                    metadata.insert_bin("error-bin", MetadataValue::from_bytes(b"details"));
                    Err(Status::with_details_and_metadata(
                        Code::Unavailable,
                        "tonic down: 50% ☃",
                        bytes::Bytes::from_static(b"opaque"),
                        metadata,
                    ))
                }
            }
        })
    }
}

type TonicResponseStream = Pin<Box<dyn Stream<Item = Result<TestResponse, Status>> + Send>>;

#[derive(Clone)]
struct TonicServerStreaming {
    behavior: Arc<TonicBehavior>,
}

impl tonic::server::ServerStreamingService<TestRequest> for TonicServerStreaming {
    type Response = TestResponse;
    type ResponseStream = TonicResponseStream;
    type Future = BoxFuture<Response<Self::ResponseStream>, Status>;

    fn call(&mut self, request: Request<TestRequest>) -> Self::Future {
        let behavior = self.behavior.clone();
        Box::pin(async move {
            assert_eq!(request.metadata().get("x-request").unwrap(), "stackful");
            assert_eq!(
                request
                    .metadata()
                    .get_bin("trace-bin")
                    .unwrap()
                    .to_bytes()
                    .unwrap()
                    .as_ref(),
                b"trace"
            );

            let mut response = match &*behavior {
                TonicBehavior::Success => {
                    let value = request.get_ref().value.clone();
                    Response::new(Box::pin(tonic::codegen::tokio_stream::iter([
                        Ok(TestResponse {
                            value: format!("tonic {value} one"),
                        }),
                        Ok(TestResponse {
                            value: format!("tonic {value} two"),
                        }),
                    ])) as TonicResponseStream)
                }
                TonicBehavior::Error => {
                    let value = request.get_ref().value.clone();
                    let mut metadata = tonic::metadata::MetadataMap::new();
                    metadata.insert("x-error", MetadataValue::from_static("tonic"));
                    metadata.insert_bin("error-bin", MetadataValue::from_bytes(b"details"));
                    Response::new(Box::pin(tonic::codegen::tokio_stream::iter([
                        Ok(TestResponse {
                            value: format!("tonic {value} before-error"),
                        }),
                        Err(Status::with_details_and_metadata(
                            Code::Unavailable,
                            "tonic stream down",
                            bytes::Bytes::from_static(b"stream-opaque"),
                            metadata,
                        )),
                    ])) as TonicResponseStream)
                }
            };
            response
                .metadata_mut()
                .insert("x-tonic", MetadataValue::from_static("yes"));
            Ok(response)
        })
    }
}

#[derive(Clone)]
struct TonicClientStreaming {
    behavior: Arc<TonicBehavior>,
}

impl tonic::server::ClientStreamingService<TestRequest> for TonicClientStreaming {
    type Response = TestResponse;
    type Future = BoxFuture<Response<Self::Response>, Status>;

    fn call(&mut self, request: Request<tonic::Streaming<TestRequest>>) -> Self::Future {
        let behavior = self.behavior.clone();
        Box::pin(async move {
            assert_eq!(request.metadata().get("x-request").unwrap(), "stackful");
            assert_eq!(
                request
                    .metadata()
                    .get_bin("trace-bin")
                    .unwrap()
                    .to_bytes()
                    .unwrap()
                    .as_ref(),
                b"trace"
            );

            let mut stream = request.into_inner();
            let mut values = Vec::new();
            while let Some(request) = stream.message().await? {
                values.push(request.value);
            }

            match &*behavior {
                TonicBehavior::Success => {
                    let mut response = Response::new(TestResponse {
                        value: format!("tonic {}", values.join("+")),
                    });
                    response
                        .metadata_mut()
                        .insert("x-tonic", MetadataValue::from_static("yes"));
                    response
                        .metadata_mut()
                        .insert_bin("trace-bin", MetadataValue::from_bytes(b"tonic-response"));
                    Ok(response)
                }
                TonicBehavior::Error => {
                    let mut metadata = tonic::metadata::MetadataMap::new();
                    metadata.insert("x-error", MetadataValue::from_static("tonic"));
                    metadata.insert_bin("error-bin", MetadataValue::from_bytes(b"details"));
                    Err(Status::with_details_and_metadata(
                        Code::Unavailable,
                        "tonic client stream down",
                        bytes::Bytes::from_static(b"client-stream-opaque"),
                        metadata,
                    ))
                }
            }
        })
    }
}

pub async fn tonic_unary_call(
    channel: Channel,
    request: Request<TestRequest>,
) -> Result<Response<TestResponse>, Status> {
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready()
        .await
        .map_err(|error| Status::unavailable(error.to_string()))?;
    grpc.unary(
        request,
        PathAndQuery::from_static(UNARY_PATH),
        ProstCodec::<TestRequest, TestResponse>::default(),
    )
    .await
}

pub async fn tonic_server_streaming_call(
    channel: Channel,
    request: Request<TestRequest>,
) -> Result<Response<tonic::Streaming<TestResponse>>, Status> {
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready()
        .await
        .map_err(|error| Status::unavailable(error.to_string()))?;
    grpc.server_streaming(
        request,
        PathAndQuery::from_static(STREAM_PATH),
        ProstCodec::<TestRequest, TestResponse>::default(),
    )
    .await
}

pub async fn tonic_client_streaming_call(
    channel: Channel,
    request: Request<impl Stream<Item = TestRequest> + Send + 'static>,
) -> Result<Response<TestResponse>, Status> {
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready()
        .await
        .map_err(|error| Status::unavailable(error.to_string()))?;
    grpc.client_streaming(
        request,
        PathAndQuery::from_static(CLIENT_STREAM_PATH),
        ProstCodec::<TestRequest, TestResponse>::default(),
    )
    .await
}

pub fn tonic_request(value: &str) -> Result<Request<TestRequest>, InvalidMetadataValue> {
    let mut request = Request::new(TestRequest {
        value: value.to_owned(),
    });
    request
        .metadata_mut()
        .insert("x-request", MetadataValue::from_static("tonic"));
    request
        .metadata_mut()
        .insert_bin("trace-bin", MetadataValue::from_bytes(b"trace"));
    Ok(request)
}
