#![allow(dead_code)]

use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use kimojio_tls_pool::{
    CompletionCallback, OperationError, OperationResult, PlacementMode, PoolConfig, TlsPool,
    TlsStream,
};
use openssl::{
    asn1::Asn1Time,
    hash::MessageDigest,
    nid::Nid,
    pkey::PKey,
    rsa::Rsa,
    ssl::{SslAcceptor, SslConnector, SslMethod, SslVerifyMode},
    x509::{X509, X509NameBuilder},
};

pub const RPC_HEADER_LEN: usize = 64;
pub const RPC_RESPONSE_LEN: usize = 64;

pub fn tls_contexts() -> (SslAcceptor, SslConnector) {
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
    let cert = cert.build();

    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    acceptor.set_private_key(&key).unwrap();
    acceptor.set_certificate(&cert).unwrap();
    acceptor.check_private_key().unwrap();

    let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
    connector.set_verify(SslVerifyMode::NONE);

    (acceptor.build(), connector.build())
}

pub fn stream_pair(client_config: PoolConfig, server_config: PoolConfig) -> (TlsStream, TlsStream) {
    let (acceptor, connector) = tls_contexts();
    let (client_io, server_io) = UnixStream::pair().unwrap();
    let server_thread = thread::spawn(move || {
        let pool = TlsPool::new(server_config).unwrap();
        pool.server(&acceptor, server_io).unwrap()
    });

    let client_pool = TlsPool::new(client_config).unwrap();
    let client = client_pool
        .client(&connector, "localhost", client_io)
        .unwrap();
    let server = server_thread.join().unwrap();
    (client, server)
}

pub fn immediate_config() -> PoolConfig {
    PoolConfig::new(1).with_placement_mode(PlacementMode::ImmediateOnly)
}

pub fn background_config(executors: usize) -> PoolConfig {
    PoolConfig::new(executors).with_placement_mode(PlacementMode::BackgroundOnly)
}

pub fn write_blocking(stream: &TlsStream, bytes: &[u8]) -> OperationResult<usize> {
    let (sender, receiver) = mpsc::channel();
    stream.write(bytes.to_vec(), callback(sender))?;
    receiver.recv_timeout(Duration::from_secs(5)).unwrap()
}

pub fn read_once_blocking(stream: &TlsStream, len: usize) -> OperationResult<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    stream.read(len, callback(sender))?;
    receiver.recv_timeout(Duration::from_secs(5)).unwrap()
}

pub fn read_exact_blocking(stream: &TlsStream, len: usize) -> OperationResult<Vec<u8>> {
    let mut output = Vec::with_capacity(len);
    while output.len() < len {
        let chunk = read_once_blocking(stream, len - output.len())?;
        if chunk.is_empty() {
            return Err(OperationError::Shutdown);
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

pub fn make_rpc_header(body_len: usize) -> [u8; RPC_HEADER_LEN] {
    let mut header = [0xa5; RPC_HEADER_LEN];
    header[..size_of::<u64>()].copy_from_slice(&(body_len as u64).to_le_bytes());
    header
}

pub fn rpc_body_len(header: &[u8; RPC_HEADER_LEN]) -> usize {
    let mut len = [0_u8; size_of::<u64>()];
    len.copy_from_slice(&header[..size_of::<u64>()]);
    u64::from_le_bytes(len) as usize
}

fn callback<T>(sender: mpsc::Sender<OperationResult<T>>) -> CompletionCallback<T>
where
    T: Send + 'static,
{
    Box::new(move |result| {
        sender.send(result).unwrap();
    })
}
