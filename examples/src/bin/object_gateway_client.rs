// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use bytes::Bytes;
use clap::{Parser, Subcommand};
use examples::object_gateway::{
    proto,
    stackful::{StackfulGatewayClient, StackfulGatewayConfig},
};
use kimojio_stack::{Runtime, RuntimeContext};
use kimojio_stack_grpc::{RuntimeUnaryClient, UnaryResponse};
use kimojio_stack_http::{HttpConfig, StackTransport, h2};
use rustix::net::{AddressFamily, SocketType, ipproto};

#[derive(Debug, Parser)]
#[command(version, about = "Small Object Gateway client")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:9100")]
    grpc_addr: SocketAddr,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Health,
    Put {
        namespace: String,
        object: String,
        #[arg(long, default_value = "hello")]
        data: String,
    },
    Get {
        namespace: String,
        object: String,
    },
    Delete {
        namespace: String,
        object: String,
    },
    List {
        namespace: String,
    },
    Copy {
        namespace: String,
        source: String,
        destination: String,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut runtime = Runtime::new();
    runtime.block_on(|cx| run_command(cx, args))
}

fn run_command(cx: &RuntimeContext<'_>, args: Args) -> Result<()> {
    let transport = stack_connect(cx, args.grpc_addr)?;
    let http = h2::ClientConnection::new(transport, HttpConfig::default());
    let mut client = StackfulGatewayClient::new(RuntimeUnaryClient::new(
        http,
        kimojio_stack_grpc::ClientConfig::default(),
    ));
    match args.command {
        Command::Health => {
            let response: UnaryResponse<proto::HealthResponse> =
                client.unary(cx, proto::HEALTH_PATH, &proto::HealthRequest {})?;
            println!(
                "object-gateway-client command=health service={} ready={} version={}",
                proto::SERVICE_NAME,
                response.message.ready,
                response.message.version
            );
        }
        Command::Put {
            namespace,
            object,
            data,
        } => {
            let response = client.put(cx, &namespace, &object, vec![Bytes::from(data)])?;
            let status = response.message.status.unwrap_or_else(ok_status);
            let size_bytes = response
                .message
                .metadata
                .as_ref()
                .map(|metadata| metadata.size_bytes)
                .unwrap_or_default();
            println!(
                "object-gateway-client command=put status={} size_bytes={size_bytes}",
                status.code
            );
        }
        Command::Get { namespace, object } => {
            let chunks = client.get(
                cx,
                &namespace,
                &object,
                StackfulGatewayConfig::default().size_limits.max_chunk_bytes as u32,
            )?;
            let status = chunks
                .first()
                .and_then(|chunk| chunk.status.clone())
                .unwrap_or_else(ok_status);
            let data = chunks
                .iter()
                .flat_map(|chunk| chunk.data.iter().copied())
                .collect::<Vec<_>>();
            println!(
                "object-gateway-client command=get status={} size_bytes={} data={}",
                status.code,
                data.len(),
                String::from_utf8_lossy(&data)
            );
        }
        Command::Delete { namespace, object } => {
            let response: UnaryResponse<proto::DeleteResponse> = client.unary(
                cx,
                proto::DELETE_PATH,
                &proto::DeleteRequest {
                    object: Some(proto::ObjectIdentity {
                        namespace,
                        name: object,
                    }),
                },
            )?;
            let status = response.message.status.unwrap_or_else(ok_status);
            println!(
                "object-gateway-client command=delete status={}",
                status.code
            );
        }
        Command::List { namespace } => {
            let response: UnaryResponse<proto::ListResponse> = client.unary(
                cx,
                proto::LIST_PATH,
                &proto::ListRequest {
                    namespace,
                    prefix: String::new(),
                    max_results: 0,
                },
            )?;
            let status = response.message.status.unwrap_or_else(ok_status);
            let names = response
                .message
                .objects
                .iter()
                .filter_map(|metadata| metadata.object.as_ref())
                .map(|object| object.name.as_str())
                .collect::<Vec<_>>()
                .join(",");
            println!(
                "object-gateway-client command=list status={} objects={names}",
                status.code
            );
        }
        Command::Copy {
            namespace,
            source,
            destination,
        } => {
            let response: UnaryResponse<proto::CopyResponse> = client.unary(
                cx,
                proto::COPY_PATH,
                &proto::CopyRequest {
                    source: Some(proto::ObjectIdentity {
                        namespace: namespace.clone(),
                        name: source,
                    }),
                    destination: Some(proto::ObjectIdentity {
                        namespace,
                        name: destination,
                    }),
                },
            )?;
            let status = response.message.status.unwrap_or_else(ok_status);
            println!("object-gateway-client command=copy status={}", status.code);
        }
    }
    client.close(cx)?;
    Ok(())
}

fn stack_connect(cx: &RuntimeContext<'_>, addr: SocketAddr) -> Result<StackTransport> {
    let socket = cx
        .socket(address_family(addr), SocketType::STREAM, Some(ipproto::TCP))
        .context("socket creation failed")?;
    cx.connect(&socket, &addr).context("connect failed")?;
    Ok(StackTransport::plaintext(socket))
}

fn address_family(addr: SocketAddr) -> AddressFamily {
    if addr.is_ipv4() {
        AddressFamily::INET
    } else {
        AddressFamily::INET6
    }
}

fn ok_status() -> proto::OperationStatus {
    proto::OperationStatus {
        code: proto::ObjectStatusCode::Ok as i32,
        message: String::new(),
        storage_class: proto::StorageStatusClass::None as i32,
    }
}
