// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod interop_proto;

use std::{net::Ipv4Addr, sync::mpsc};

use http::HeaderName;
use interop_proto::{TestRequest, TonicBehavior, TonicTestServer};
use kimojio_stack::Runtime;
use kimojio_stack_grpc::{Metadata, StatusCode, UnaryClient, client::ClientConfig};
use kimojio_stack_http::{
    HttpConfig, h2,
    tls::{self, HttpProtocol},
};
use kimojio_stack_tls::TlsContext;
use openssl::{
    asn1::Asn1Time,
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    rsa::Rsa,
    ssl::{SslConnector, SslMethod, SslVerifyMode},
    x509::{X509, X509NameBuilder},
};
use tokio::{net::TcpListener, sync::oneshot};
use tonic::transport::{Identity, Server, ServerTlsConfig};

const TLS_BUFFER_SIZE: usize = 16 * 1024;

#[test]
fn stackful_grpc_client_calls_tonic_server_over_tls() {
    let material = tls_material();
    let (addr_tx, addr_rx) = mpsc::channel();
    let (server, shutdown) = spawn_tonic_tls_server(material.cert_pem, material.key_pem, addr_tx);
    let addr = addr_rx.recv().unwrap();

    let response = Runtime::new().block_on(|cx| {
        let socket = cx
            .socket(
                rustix::net::AddressFamily::INET,
                rustix::net::SocketType::STREAM,
                Some(rustix::net::ipproto::TCP),
            )
            .unwrap();
        cx.connect(&socket, &addr).unwrap();
        let transport = tls::client_transport(
            cx,
            &material.client,
            TLS_BUFFER_SIZE,
            socket,
            "localhost",
            Some(HttpProtocol::Http2),
        )
        .unwrap();
        let http = h2::ClientConnection::new(transport, HttpConfig::default());
        let mut client = UnaryClient::new(http, ClientConfig::default());
        let mut metadata = Metadata::new();
        metadata
            .insert(
                HeaderName::from_static("x-request"),
                http::HeaderValue::from_static("stackful"),
            )
            .unwrap();
        metadata
            .insert_bin(HeaderName::from_static("trace-bin"), b"trace")
            .unwrap();
        let response = client
            .call::<_, interop_proto::TestResponse>(
                cx,
                interop_proto::UNARY_PATH,
                metadata,
                &TestRequest {
                    value: "tls".to_owned(),
                },
            )
            .unwrap();
        client.close(cx).unwrap();
        response
    });

    shutdown.send(()).unwrap();
    server.join().unwrap();
    assert_eq!(response.message.value, "tonic tls");
    assert_eq!(response.status.code(), StatusCode::Ok);
}

struct TlsMaterial {
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
    client: TlsContext,
}

fn tls_material() -> TlsMaterial {
    let (cert, key) = self_signed_cert();
    let cert_pem = cert.to_pem().unwrap();
    let key_pem = key.private_key_to_pem_pkcs8().unwrap();

    let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
    connector.set_verify(SslVerifyMode::NONE);
    connector.set_alpn_protos(b"\x02h2").unwrap();

    TlsMaterial {
        cert_pem,
        key_pem,
        client: TlsContext::from_openssl(connector.build().into_context()),
    }
}

fn spawn_tonic_tls_server(
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
    addr_tx: mpsc::Sender<std::net::SocketAddr>,
) -> (std::thread::JoinHandle<()>, oneshot::Sender<()>) {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let handle = std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                let identity = Identity::from_pem(cert_pem, key_pem);
                Server::builder()
                    .tls_config(ServerTlsConfig::new().identity(identity))
                    .unwrap()
                    .add_service(TonicTestServer::new(TonicBehavior::Success))
                    .serve_with_incoming_shutdown(
                        tonic::codegen::tokio_stream::wrappers::TcpListenerStream::new(listener),
                        async {
                            let _ = shutdown_rx.await;
                        },
                    )
                    .await
                    .unwrap();
            });
    });
    (handle, shutdown_tx)
}

fn self_signed_cert() -> (X509, PKey<Private>) {
    let rsa = Rsa::generate(2048).unwrap();
    let key = PKey::from_rsa(rsa).unwrap();

    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_nid(Nid::COMMONNAME, "localhost")
        .unwrap();
    let name = name.build();

    let mut cert = X509::builder().unwrap();
    cert.set_version(2).unwrap();
    cert.set_subject_name(&name).unwrap();
    cert.set_issuer_name(&name).unwrap();
    cert.set_pubkey(&key).unwrap();
    cert.set_not_before(Asn1Time::days_from_now(0).unwrap().as_ref())
        .unwrap();
    cert.set_not_after(Asn1Time::days_from_now(1).unwrap().as_ref())
        .unwrap();
    cert.sign(&key, MessageDigest::sha256()).unwrap();
    (cert.build(), key)
}
