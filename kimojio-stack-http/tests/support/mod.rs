// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(dead_code)]

use std::{
    future::Future,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    thread,
};

use http::{Request, Response};
use kimojio_stack::{Runtime, RuntimeContext};
use kimojio_stack_http::{
    Body, BodyLimits, StackTransport,
    h2::{ConnectionState, Frame, FrameFlags, Header, Setting, SettingId, Settings},
};
use kimojio_stack_tls::TlsContext;
use openssl::{
    asn1::Asn1Time,
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    rsa::Rsa,
    ssl::{AlpnError, SslAcceptor, SslConnector, SslMethod, SslVerifyMode, select_next_proto},
    x509::{X509, X509NameBuilder},
};
use rustix::net::{self, AddressFamily, SocketFlags, SocketType, ipproto, socketpair};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

pub const TLS_BUFFER_SIZE: usize = 16 * 1024;

pub fn body(bytes: &[u8]) -> Body {
    Body::copy_from_slice(bytes, BodyLimits::new(64 * 1024)).unwrap()
}

pub fn run_stackful<T>(f: impl FnOnce(&RuntimeContext<'_>) -> T) -> T {
    Runtime::new().block_on(f)
}

pub fn spawn_tokio<F>(future: F) -> thread::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    })
}

pub fn stack_connect(cx: &RuntimeContext<'_>, addr: SocketAddr) -> StackTransport {
    let socket = cx
        .socket(AddressFamily::INET, SocketType::STREAM, Some(ipproto::TCP))
        .unwrap();
    cx.connect(&socket, &addr).unwrap();
    StackTransport::plaintext(socket)
}

pub fn stack_listener(cx: &RuntimeContext<'_>) -> (rustix::fd::OwnedFd, SocketAddr) {
    let listener = cx
        .socket(AddressFamily::INET, SocketType::STREAM, Some(ipproto::TCP))
        .unwrap();
    let loopback = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
    cx.bind(&listener, &loopback).unwrap();
    cx.listen(&listener, 8).unwrap();
    let addr: SocketAddr = net::getsockname(&listener).unwrap().try_into().unwrap();
    (listener, addr)
}

pub fn socket_transport_pair() -> (StackTransport, StackTransport) {
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

pub fn tls_socket_pair() -> (rustix::fd::OwnedFd, rustix::fd::OwnedFd) {
    socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .unwrap()
}

pub struct TlsContexts {
    pub server: TlsContext,
    pub client: TlsContext,
}

pub fn tls_contexts(alpn: Option<&'static [u8]>, verify_peer: bool) -> TlsContexts {
    let (cert, key) = self_signed_cert();
    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    acceptor.set_private_key(&key).unwrap();
    acceptor.set_certificate(&cert).unwrap();
    acceptor.check_private_key().unwrap();
    if let Some(protos) = alpn {
        acceptor.set_alpn_select_callback(move |_ssl, client| {
            select_next_proto(protos, client).ok_or(AlpnError::NOACK)
        });
    }

    let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
    if verify_peer {
        connector.set_verify(SslVerifyMode::PEER);
    } else {
        connector.set_verify(SslVerifyMode::NONE);
    }
    if let Some(protos) = alpn {
        connector.set_alpn_protos(protos).unwrap();
    }

    TlsContexts {
        server: TlsContext::from_openssl(acceptor.build().into_context()),
        client: TlsContext::from_openssl(connector.build().into_context()),
    }
}

pub fn h2_alpn() -> &'static [u8] {
    b"\x02h2"
}

pub fn http1_alpn() -> &'static [u8] {
    b"\x08http/1.1"
}

pub async fn read_h2_frame(stream: &mut TcpStream, max_frame_size: u32) -> Frame {
    let mut header = [0_u8; 9];
    stream.read_exact(&mut header).await.unwrap();
    let len = ((header[0] as usize) << 16) | ((header[1] as usize) << 8) | header[2] as usize;
    let mut bytes = Vec::with_capacity(9 + len);
    bytes.extend_from_slice(&header);
    bytes.resize(9 + len, 0);
    if len > 0 {
        stream.read_exact(&mut bytes[9..]).await.unwrap();
    }
    Frame::decode(&bytes, max_frame_size).unwrap().0
}

pub async fn write_h2_frame(stream: &mut TcpStream, frame: &Frame) {
    stream.write_all(&frame.encode().unwrap()).await.unwrap();
}

pub async fn read_h2_preface(stream: &mut TcpStream) {
    let mut preface = [0_u8; kimojio_stack_http::h2::CLIENT_PREFACE.len()];
    stream.read_exact(&mut preface).await.unwrap();
    assert_eq!(&preface, kimojio_stack_http::h2::CLIENT_PREFACE);
}

pub async fn write_h2_preface(stream: &mut TcpStream) {
    stream
        .write_all(kimojio_stack_http::h2::CLIENT_PREFACE)
        .await
        .unwrap();
}

pub fn settings_frame(settings: Settings) -> Frame {
    Frame::settings(vec![
        Setting::new(SettingId::HeaderTableSize, settings.header_table_size),
        Setting::new(SettingId::EnablePush, u32::from(settings.enable_push)),
        Setting::new(SettingId::InitialWindowSize, settings.initial_window_size),
        Setting::new(SettingId::MaxFrameSize, settings.max_frame_size),
        Setting::new(
            SettingId::MaxConcurrentStreams,
            settings.max_concurrent_streams,
        ),
        Setting::new(SettingId::MaxHeaderListSize, settings.max_header_list_size),
    ])
}

pub async fn h2_server_handshake(stream: &mut TcpStream, state: &mut ConnectionState) {
    read_h2_preface(stream).await;
    let settings = read_h2_frame(stream, state.local_settings().max_frame_size).await;
    state.receive_settings(&settings).unwrap();
    write_h2_frame(stream, &settings_frame(state.local_settings())).await;
    while let Some(frame) = state.pop_outbound() {
        write_h2_frame(stream, &frame).await;
    }
}

pub async fn h2_client_handshake(stream: &mut TcpStream, state: &mut ConnectionState) {
    write_h2_preface(stream).await;
    write_h2_frame(stream, &settings_frame(state.local_settings())).await;
    let mut saw_server_settings = false;
    while !saw_server_settings {
        let frame = read_h2_frame(stream, state.local_settings().max_frame_size).await;
        if frame.frame_type == kimojio_stack_http::h2::FrameType::Settings {
            let is_ack = frame.flags.contains(FrameFlags::ACK);
            state.receive_settings(&frame).unwrap();
            while let Some(frame) = state.pop_outbound() {
                write_h2_frame(stream, &frame).await;
            }
            saw_server_settings |= !is_ack;
        }
    }
}

pub fn encode_headers(state: &mut ConnectionState, headers: &[(&str, &str)]) -> Vec<u8> {
    state.encode_header_block(
        &headers
            .iter()
            .map(|(name, value)| Header::new(name.as_bytes().to_vec(), value.as_bytes().to_vec()))
            .collect::<Vec<_>>(),
    )
}

pub fn decode_headers(state: &mut ConnectionState, block: &[u8]) -> Vec<Header> {
    state.decode_header_block(block).unwrap()
}

pub fn response(status: u16, bytes: &[u8]) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(body(bytes))
        .unwrap()
}

pub fn request(method: &str, uri: &str, bytes: &[u8]) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(body(bytes))
        .unwrap()
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
