// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serves a document root over cleartext and TLS HTTP/1.1 and HTTP/2.
//!
//! Directory requests serve `index.html`; directories without an index and
//! missing files return 404. Paths that escape the document root return 403.

use std::borrow::Cow;
use std::ffi::CString;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use kimojio::http::{
    AlpnProtocol, Body, HeaderValue, Limits, Method, Request, Response, Server, StatusCode,
    TlsServerConfig,
};
use kimojio::{CancellationToken, operations};
use openssl::ssl::{SslContextBuilder, SslFiletype, SslMethod};
use rustix::fs::Mode;

const DEFAULT_MAX_FILE_SIZE: usize = 64 * 1024 * 1024;
const MIN_HTTP_BODY_LIMIT: usize = 64;
const SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const TEXT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Serve static files over cleartext and TLS HTTP/1.1 and HTTP/2"
)]
struct Args {
    /// Document root to serve
    #[arg(long, value_name = "DIR")]
    root: PathBuf,

    /// Cleartext listen address
    #[arg(long, value_name = "ADDR")]
    listen: Option<SocketAddr>,

    /// TLS listen address
    #[arg(long, value_name = "ADDR")]
    tls_listen: Option<SocketAddr>,

    /// PEM certificate chain for the TLS listener
    #[arg(long, value_name = "FILE")]
    cert: Option<PathBuf>,

    /// PEM private key for the TLS listener
    #[arg(long, value_name = "FILE")]
    key: Option<PathBuf>,

    /// Maximum file size to serve, in bytes
    #[arg(long, default_value_t = DEFAULT_MAX_FILE_SIZE, value_name = "BYTES")]
    max_file_size: usize,
}

#[derive(Debug)]
struct App {
    root: PathBuf,
    max_file_size: usize,
}

#[derive(Debug, Eq, PartialEq)]
enum ResolveError {
    BadRequest,
    Forbidden,
    NotFound,
    Io,
}

#[derive(Debug)]
enum ReadError {
    Forbidden,
    NotFound,
    Io,
}

#[derive(Debug, Eq, PartialEq)]
struct ResolvedFile {
    path: PathBuf,
    size: usize,
}

extern "C" fn shutdown_signal_handler(_signal: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
}

fn install_signal_handlers() -> Result<()> {
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = shutdown_signal_handler as usize;
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
        return Err(io::Error::last_os_error()).context("failed to initialize signal mask");
    }

    for signal in [libc::SIGINT, libc::SIGTERM] {
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("failed to install handler for signal {signal}"));
        }
    }
    Ok(())
}

async fn cancel_on_signal(cancellation: Rc<CancellationToken>) {
    while !SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
        if operations::sleep(SIGNAL_POLL_INTERVAL).await.is_err() {
            return;
        }
    }
    cancellation.cancel();
}

fn validate_args(args: &Args) -> Result<()> {
    if args.listen.is_none() && args.tls_listen.is_none() {
        bail!("at least one of --listen or --tls-listen must be provided");
    }
    if args.max_file_size == 0 {
        bail!("--max-file-size must be greater than zero");
    }
    if args.tls_listen.is_some() {
        if args.cert.is_none() {
            bail!("--cert is required when --tls-listen is provided");
        }
        if args.key.is_none() {
            bail!("--key is required when --tls-listen is provided");
        }
    } else if args.cert.is_some() || args.key.is_some() {
        bail!("--cert and --key require --tls-listen");
    }
    Ok(())
}

fn canonical_document_root(root: &Path) -> Result<PathBuf> {
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize document root {}", root.display()))?;
    if !root.is_dir() {
        bail!("document root is not a directory: {}", root.display());
    }
    Ok(root)
}

fn tls_config(cert: &Path, key: &Path) -> Result<TlsServerConfig> {
    let mut builder =
        SslContextBuilder::new(SslMethod::tls_server()).context("failed to create TLS context")?;
    builder
        .set_certificate_chain_file(cert)
        .with_context(|| format!("failed to load certificate file {}", cert.display()))?;
    builder
        .set_private_key_file(key, SslFiletype::PEM)
        .with_context(|| format!("failed to load private key file {}", key.display()))?;
    builder
        .check_private_key()
        .context("private key does not match certificate")?;
    TlsServerConfig::new(builder, &[AlpnProtocol::H2, AlpnProtocol::Http11])
        .context("failed to configure HTTP ALPN")
}

fn decode_request_path(path: &str) -> Result<Cow<'_, str>, ResolveError> {
    let bytes = path.as_bytes();
    if !bytes.contains(&b'%') {
        return Ok(Cow::Borrowed(path));
    }
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(ResolveError::BadRequest);
            }
            let high = hex_value(bytes[index + 1]).ok_or(ResolveError::BadRequest)?;
            let low = hex_value(bytes[index + 2]).ok_or(ResolveError::BadRequest)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .map(Cow::Owned)
        .map_err(|_| ResolveError::BadRequest)
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn resolve_file(root: &Path, request_path: &str) -> Result<ResolvedFile, ResolveError> {
    let decoded = decode_request_path(request_path)?;
    let relative = decoded.strip_prefix('/').ok_or(ResolveError::BadRequest)?;
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ResolveError::Forbidden);
    }

    let mut target = canonicalize_target(&root.join(relative))?;
    if !target.starts_with(root) {
        return Err(ResolveError::Forbidden);
    }
    let mut metadata = target.metadata().map_err(classify_io_error)?;
    if metadata.is_dir() {
        target = canonicalize_target(&target.join("index.html"))?;
        if !target.starts_with(root) {
            return Err(ResolveError::Forbidden);
        }
        metadata = target.metadata().map_err(classify_io_error)?;
    }

    if !metadata.is_file() {
        return Err(ResolveError::NotFound);
    }
    let size = usize::try_from(metadata.len()).map_err(|_| ResolveError::Io)?;
    Ok(ResolvedFile { path: target, size })
}

fn canonicalize_target(path: &Path) -> Result<PathBuf, ResolveError> {
    path.canonicalize().map_err(classify_io_error)
}

fn classify_io_error(error: io::Error) -> ResolveError {
    match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => ResolveError::NotFound,
        io::ErrorKind::PermissionDenied => ResolveError::Forbidden,
        _ => ResolveError::Io,
    }
}

fn content_type(path: &Path) -> &'static str {
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return "application/octet-stream";
    };
    if extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm") {
        "text/html; charset=utf-8"
    } else if extension.eq_ignore_ascii_case("css") {
        "text/css; charset=utf-8"
    } else if extension.eq_ignore_ascii_case("js") || extension.eq_ignore_ascii_case("mjs") {
        "text/javascript; charset=utf-8"
    } else if extension.eq_ignore_ascii_case("json") {
        "application/json"
    } else if extension.eq_ignore_ascii_case("txt") {
        TEXT_CONTENT_TYPE
    } else if extension.eq_ignore_ascii_case("svg") {
        "image/svg+xml"
    } else if extension.eq_ignore_ascii_case("png") {
        "image/png"
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        "image/jpeg"
    } else if extension.eq_ignore_ascii_case("gif") {
        "image/gif"
    } else if extension.eq_ignore_ascii_case("ico") {
        "image/vnd.microsoft.icon"
    } else if extension.eq_ignore_ascii_case("wasm") {
        "application/wasm"
    } else if extension.eq_ignore_ascii_case("pdf") {
        "application/pdf"
    } else if extension.eq_ignore_ascii_case("xml") {
        "application/xml"
    } else {
        "application/octet-stream"
    }
}

async fn read_file(file: &ResolvedFile) -> Result<Vec<u8>, ReadError> {
    let path = CString::new(file.path.as_os_str().as_bytes()).map_err(|_| ReadError::Io)?;
    let descriptor = operations::open(&path, operations::OFlags::RDONLY, Mode::empty())
        .await
        .map_err(classify_read_error)?;
    let mut body = Vec::new();
    body.try_reserve_exact(file.size)
        .map_err(|_| ReadError::Io)?;
    body.resize(file.size, 0);

    let mut read = 0;
    while read < body.len() {
        let end = body.len().min(read.saturating_add(u32::MAX as usize));
        let amount = operations::read(&descriptor, &mut body[read..end])
            .await
            .map_err(classify_read_error)?;
        if amount == 0 {
            body.truncate(read);
            break;
        }
        read += amount;
    }
    Ok(body)
}

fn classify_read_error(error: kimojio::Errno) -> ReadError {
    match error.raw_os_error() {
        libc::EACCES | libc::EPERM => ReadError::Forbidden,
        libc::ENOENT | libc::ENOTDIR => ReadError::NotFound,
        _ => ReadError::Io,
    }
}

fn response(status: StatusCode, content_type: &'static str, body: Vec<u8>) -> Response<Body> {
    let content_length = HeaderValue::from_str(&body.len().to_string())
        .expect("a decimal usize is a valid header value");
    let mut response = Response::new(Body::new(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert("content-length", content_length);
    response
}

fn text_response(status: StatusCode, message: &'static str) -> Response<Body> {
    response(status, TEXT_CONTENT_TYPE, message.as_bytes().to_vec())
}

async fn handle_request(request: Request<Body>, app: Rc<App>) -> Response<Body> {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        let mut response = text_response(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed\n");
        response
            .headers_mut()
            .insert("allow", HeaderValue::from_static("GET, HEAD"));
        return response;
    }

    let file = match resolve_file(&app.root, request.uri().path()) {
        Ok(file) => file,
        Err(ResolveError::BadRequest) => {
            return text_response(StatusCode::BAD_REQUEST, "Bad Request\n");
        }
        Err(ResolveError::Forbidden) => {
            return text_response(StatusCode::FORBIDDEN, "Forbidden\n");
        }
        Err(ResolveError::NotFound) => {
            return text_response(StatusCode::NOT_FOUND, "Not Found\n");
        }
        Err(ResolveError::Io) => {
            return text_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error\n");
        }
    };
    if file.size > app.max_file_size {
        return text_response(StatusCode::PAYLOAD_TOO_LARGE, "File Too Large\n");
    }

    let media_type = content_type(&file.path);
    match read_file(&file).await {
        Ok(body) => response(StatusCode::OK, media_type, body),
        Err(ReadError::Forbidden) => text_response(StatusCode::FORBIDDEN, "Forbidden\n"),
        Err(ReadError::NotFound) => text_response(StatusCode::NOT_FOUND, "Not Found\n"),
        Err(ReadError::Io) => {
            text_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error\n")
        }
    }
}

async fn serve_listener(
    server: Server,
    app: Rc<App>,
    cancellation: Rc<CancellationToken>,
) -> Result<(), kimojio::http::Error> {
    let cancellation_for_serve = Rc::clone(&cancellation);
    let result = server
        .serve(
            move |request| {
                let app = Rc::clone(&app);
                async move { handle_request(request, app).await }
            },
            cancellation_for_serve,
        )
        .await;
    cancellation.cancel();
    result
}

#[kimojio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;
    install_signal_handlers()?;

    let root = canonical_document_root(&args.root)?;
    let app = Rc::new(App {
        root,
        max_file_size: args.max_file_size,
    });
    let limits = Limits::new().set_max_body_bytes(args.max_file_size.max(MIN_HTTP_BODY_LIMIT));

    let plain_server = match args.listen {
        Some(address) => Some(
            Server::builder(address)
                .limits(limits)
                .bind()
                .await
                .with_context(|| format!("failed to bind cleartext listener {address}"))?,
        ),
        None => None,
    };
    let tls_server = match args.tls_listen {
        Some(address) => {
            let tls = tls_config(
                args.cert.as_deref().expect("validated certificate path"),
                args.key.as_deref().expect("validated private key path"),
            )?;
            Some(
                Server::builder(address)
                    .limits(limits)
                    .tls(tls)
                    .bind()
                    .await
                    .with_context(|| format!("failed to bind TLS listener {address}"))?,
            )
        }
        None => None,
    };

    if let Some(server) = &plain_server {
        println!("listening plain http://{}", server.local_addr());
    }
    if let Some(server) = &tls_server {
        println!("listening tls https://{}", server.local_addr());
    }
    io::stdout()
        .flush()
        .context("failed to flush readiness lines")?;

    let cancellation = Rc::new(CancellationToken::new());
    let _signal_task = operations::spawn_task(cancel_on_signal(Rc::clone(&cancellation)));

    match (plain_server, tls_server) {
        (Some(plain), Some(tls)) => {
            let (plain_result, tls_result) = futures::join!(
                serve_listener(plain, Rc::clone(&app), Rc::clone(&cancellation)),
                serve_listener(tls, app, cancellation)
            );
            plain_result?;
            tls_result?;
        }
        (Some(plain), None) => serve_listener(plain, app, cancellation).await?,
        (None, Some(tls)) => serve_listener(tls, app, cancellation).await?,
        (None, None) => unreachable!("listener presence validated"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let base = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../target/static-file-server-tests")
                .join(format!("{}-{sequence}", std::process::id()));
            let root = base.join("root");
            fs::create_dir_all(&root).unwrap();
            Self {
                root: root.canonicalize().unwrap(),
                base,
            }
        }

        fn app(&self) -> Rc<App> {
            Rc::new(App {
                root: self.root.clone(),
                max_file_size: DEFAULT_MAX_FILE_SIZE,
            })
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.base).unwrap();
        }
    }

    fn request(method: Method, path: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn infers_all_supported_content_types_case_insensitively() {
        for (extension, expected) in [
            ("html", "text/html; charset=utf-8"),
            ("htm", "text/html; charset=utf-8"),
            ("css", "text/css; charset=utf-8"),
            ("js", "text/javascript; charset=utf-8"),
            ("mjs", "text/javascript; charset=utf-8"),
            ("json", "application/json"),
            ("txt", "text/plain; charset=utf-8"),
            ("svg", "image/svg+xml"),
            ("png", "image/png"),
            ("jpg", "image/jpeg"),
            ("jpeg", "image/jpeg"),
            ("gif", "image/gif"),
            ("ico", "image/vnd.microsoft.icon"),
            ("wasm", "application/wasm"),
            ("pdf", "application/pdf"),
            ("xml", "application/xml"),
        ] {
            assert_eq!(
                content_type(Path::new(&format!("asset.{extension}"))),
                expected
            );
            assert_eq!(
                content_type(Path::new(&format!("asset.{}", extension.to_uppercase()))),
                expected
            );
        }
        assert_eq!(
            content_type(Path::new("asset.unknown")),
            "application/octet-stream"
        );
        assert_eq!(
            content_type(Path::new("extensionless")),
            "application/octet-stream"
        );
    }

    #[test]
    fn resolves_normal_nested_and_percent_encoded_paths() {
        let fixture = Fixture::new();
        let nested = fixture.root.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("hello world.txt"), b"hello").unwrap();
        fs::write(fixture.root.join("index.html"), b"index").unwrap();

        let resolved = resolve_file(&fixture.root, "/%6eested/hello%20world.txt").unwrap();
        assert_eq!(
            resolved.path,
            nested.join("hello world.txt").canonicalize().unwrap()
        );
        assert_eq!(resolved.size, 5);
        assert_eq!(
            resolve_file(&fixture.root, "/").unwrap().path,
            fixture.root.join("index.html").canonicalize().unwrap()
        );
    }

    #[test]
    fn rejects_traversal_absolute_paths_and_escaping_symlinks() {
        let fixture = Fixture::new();
        let outside = fixture.base.join("outside.txt");
        fs::write(&outside, b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, fixture.root.join("escape")).unwrap();

        for path in [
            "/../outside.txt",
            "/nested/../../outside.txt",
            "/%2e%2e%2foutside.txt",
            "//etc/passwd",
            "/%2Fetc/passwd",
            "/escape",
        ] {
            assert_eq!(
                resolve_file(&fixture.root, path),
                Err(ResolveError::Forbidden),
                "{path}"
            );
        }
    }

    #[test]
    fn directories_without_an_index_return_not_found() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.root.join("empty")).unwrap();
        assert_eq!(
            resolve_file(&fixture.root, "/empty"),
            Err(ResolveError::NotFound)
        );
    }

    fn send_http1(address: SocketAddr, method: &str) -> Vec<u8> {
        let mut stream = TcpStream::connect(address).unwrap();
        write!(
            stream,
            "{method} /file.txt HTTP/1.1\r\nhost: localhost\r\n\r\n"
        )
        .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    }

    fn split_http1_response(response: &[u8]) -> (&[u8], &[u8]) {
        let body_start = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
            .unwrap();
        response.split_at(body_start)
    }

    #[kimojio::test]
    async fn head_has_get_headers_and_no_wire_body() {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("file.txt"), b"body").unwrap();
        let server = Server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let address = server.local_addr();
        let cancellation = Rc::new(CancellationToken::new());
        let calls = Rc::new(Cell::new(0));
        let app = fixture.app();
        let cancellation_from_handler = Rc::clone(&cancellation);
        let serve_task = operations::spawn_task(server.serve(
            move |request| {
                let app = Rc::clone(&app);
                let calls = Rc::clone(&calls);
                let cancellation = Rc::clone(&cancellation_from_handler);
                async move {
                    let response = handle_request(request, app).await;
                    calls.set(calls.get() + 1);
                    if calls.get() == 2 {
                        cancellation.cancel();
                    }
                    response
                }
            },
            cancellation,
        ));

        let get = std::thread::spawn(move || send_http1(address, "GET"));
        let head = std::thread::spawn(move || send_http1(address, "HEAD"));
        serve_task.await.unwrap().unwrap();
        let get = get.join().unwrap();
        let head = head.join().unwrap();
        let (get_headers, get_body) = split_http1_response(&get);
        let (head_headers, head_body) = split_http1_response(&head);
        assert_eq!(head_headers, get_headers);
        assert_eq!(get_body, b"body");
        assert!(head_body.is_empty());
    }

    #[kimojio::test]
    async fn unsupported_methods_return_405_with_allow_header() {
        let fixture = Fixture::new();
        let response = handle_request(request(Method::POST, "/file.txt"), fixture.app()).await;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()["allow"], "GET, HEAD");
    }
}
