// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{
    net::{SocketAddr, TcpStream},
    os::fd::OwnedFd,
    time::{Duration, SystemTime},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use kimojio_stack_storage::{
    DeterministicEmulator, OperationClass, ReplayBody, RequestParts, StackHttpTransport, Transport,
};
use openssl::{hash::MessageDigest, pkey::PKey, sign::Signer};

#[test]
fn emulator_gate_is_explicit_and_default_safe() {
    if std::env::var_os("KIMOJIO_STACK_STORAGE_EMULATOR").is_none() {
        eprintln!("skipping emulator test: KIMOJIO_STACK_STORAGE_EMULATOR is not set");
        return;
    }

    let addr = std::env::var("KIMOJIO_STACK_STORAGE_EMULATOR_ADDR")
        .expect("set KIMOJIO_STACK_STORAGE_EMULATOR_ADDR=host:port when enabling emulator tests");
    let account = std::env::var("KIMOJIO_STACK_STORAGE_EMULATOR_ACCOUNT")
        .unwrap_or_else(|_| "devstoreaccount1".into());
    let key = std::env::var("KIMOJIO_STACK_STORAGE_EMULATOR_KEY")
        .expect("set KIMOJIO_STACK_STORAGE_EMULATOR_KEY when enabling emulator tests");
    let addr: SocketAddr = addr.parse().expect("emulator address must be host:port");
    TcpStream::connect_timeout(&addr, Duration::from_secs(1))
        .expect("enabled emulator test could not connect to configured emulator address");

    let emulator = DeterministicEmulator {
        account: account.clone(),
        secure: false,
    };
    assert!(emulator.endpoint().contains(&account));

    let container = format!(
        "kimojio{}",
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    kimojio_stack::Runtime::new().block_on(|cx| {
        let mut transport = azurite_transport(addr);

        let create = signed_request(
            "PUT",
            &format!("/{account}/{container}?restype=container"),
            &account,
            &key,
            &[("x-ms-version", "2020-10-02")],
            ReplayBody::Empty,
        );
        let create_response = transport.execute(cx, &create).unwrap();
        assert!(matches!(create_response.status, 201 | 202));

        let upload = signed_request(
            "PUT",
            &format!("/{account}/{container}/object"),
            &account,
            &key,
            &[
                ("x-ms-version", "2020-10-02"),
                ("x-ms-blob-type", "BlockBlob"),
            ],
            ReplayBody::BorrowedStatic(b"hello"),
        );
        let upload_response = transport.execute(cx, &upload).unwrap();
        assert!(matches!(upload_response.status, 201 | 202));

        let mut chunks = Vec::new();
        let download = signed_request(
            "GET",
            &format!("/{account}/{container}/object"),
            &account,
            &key,
            &[("x-ms-version", "2020-10-02")],
            ReplayBody::Empty,
        );
        transport
            .execute_with_body_chunks(cx, &download, &mut |chunk| {
                chunks.extend_from_slice(&chunk);
                Ok(())
            })
            .unwrap();
        assert_eq!(chunks, b"hello");

        let delete = signed_request(
            "DELETE",
            &format!("/{account}/{container}?restype=container"),
            &account,
            &key,
            &[("x-ms-version", "2020-10-02")],
            ReplayBody::Empty,
        );
        let _ = transport.execute(cx, &delete);
    });
}

fn azurite_transport(addr: SocketAddr) -> StackHttpTransport {
    let stream = TcpStream::connect(addr).unwrap();
    stream.set_nodelay(true).unwrap();
    let fd: OwnedFd = stream.into();
    let client = kimojio_stack_http::client::ClientConnection::http1(
        kimojio_stack_http::StackTransport::plaintext(fd),
        kimojio_stack_http::HttpConfig::default(),
    );
    StackHttpTransport::new(client)
}

fn signed_request(
    method: &str,
    uri: &str,
    account: &str,
    key: &str,
    headers: &[(&str, &str)],
    body: ReplayBody,
) -> RequestParts {
    let mut request = RequestParts::new(OperationClass::Metadata, method, uri);
    request.body = body;
    request.metadata.insert("host", "127.0.0.1");
    request
        .metadata
        .insert("x-ms-date", httpdate::fmt_http_date(SystemTime::now()));
    for (name, value) in headers {
        request.metadata.insert(*name, *value);
    }
    if let Some(len) = request.body.len().filter(|len| *len != 0) {
        request.metadata.insert("content-length", len.to_string());
    } else {
        request.metadata.insert("content-length", "0");
    }
    let signature = shared_key_signature(method, uri, account, key, &request);
    request
        .metadata
        .insert("authorization", format!("SharedKey {account}:{signature}"));
    request
}

fn shared_key_signature(
    method: &str,
    uri: &str,
    account: &str,
    key: &str,
    request: &RequestParts,
) -> String {
    let content_length = request
        .metadata
        .get("content-length")
        .filter(|value| *value != "0")
        .unwrap_or("");
    let canonicalized_headers = request
        .metadata
        .entries()
        .iter()
        .filter(|(name, _)| name.starts_with("x-ms-"))
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect::<String>();
    let canonicalized_resource = canonicalized_resource(uri, account);
    let string_to_sign = format!(
        "{method}\n\n\n{content_length}\n\n\n\n\n\n\n\n\n{canonicalized_headers}{canonicalized_resource}"
    );
    let key = STANDARD.decode(key).expect("emulator key must be base64");
    let key = PKey::hmac(&key).unwrap();
    let mut signer = Signer::new(MessageDigest::sha256(), &key).unwrap();
    signer.update(string_to_sign.as_bytes()).unwrap();
    STANDARD.encode(signer.sign_to_vec().unwrap())
}

fn canonicalized_resource(uri: &str, account: &str) -> String {
    let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
    let mut resource = format!("/{account}{path}");
    let mut params = query
        .split('&')
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.split_once('='))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
        .collect::<Vec<_>>();
    params.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, value) in params {
        resource.push('\n');
        resource.push_str(&name);
        resource.push(':');
        resource.push_str(&value);
    }
    resource
}
