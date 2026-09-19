use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    root: PathBuf,
    address: SocketAddr,
}

impl Server {
    fn start(stop_after: usize, timeout_ms: u64) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!(
                "http1-server-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("index.html"), b"hello, io_uring\n").unwrap();
        std::fs::write(root.join("large"), vec![b'x'; 512 * 1024]).unwrap();
        std::os::unix::fs::symlink("index.html", root.join("link")).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_http1-static"))
            .args(["--bind", "127.0.0.1:0", "--root"])
            .arg(&root)
            .args([
                "--stop-after",
                &stop_after.to_string(),
                "--timeout-ms",
                &timeout_ms.to_string(),
                "--max-connections",
                "4",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let address = line
            .strip_prefix("LISTEN ")
            .expect("readiness line")
            .trim()
            .parse()
            .unwrap();
        Self {
            child,
            root,
            address,
        }
    }

    fn connect(&self) -> TcpStream {
        let socket = TcpStream::connect(self.address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket
    }

    fn finish(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "server exited with {status}");
                return;
            }
            assert!(Instant::now() < deadline, "server did not settle shutdown");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn response(stream: &mut BufReader<TcpStream>, head: bool) -> (u16, usize, Vec<u8>) {
    let mut line = String::new();
    stream.read_line(&mut line).unwrap();
    let status = line
        .split_whitespace()
        .nth(1)
        .expect("HTTP status line")
        .parse()
        .unwrap();
    let mut length = None;
    loop {
        line.clear();
        stream.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        assert!(!line.is_empty(), "EOF inside response head");
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let length = length.expect("Content-Length");
    let mut body = vec![0; if head { 0 } else { length }];
    stream.read_exact(&mut body).unwrap();
    (status, length, body)
}

#[test]
fn real_http_get_head_missing_and_pipelined_keepalive() {
    let mut server = Server::start(4, 2000);
    let mut stream = BufReader::new(server.connect());
    stream.get_mut().write_all(
        b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\nHEAD /large HTTP/1.1\r\nHost: localhost\r\n\r\nGET /missing HTTP/1.1\r\nHost: localhost\r\n\r\nGET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    ).unwrap();
    assert_eq!(
        response(&mut stream, false),
        (200, 16, b"hello, io_uring\n".to_vec())
    );
    assert_eq!(response(&mut stream, true), (200, 512 * 1024, vec![]));
    assert_eq!(response(&mut stream, false), (404, 0, vec![]));
    assert_eq!(
        response(&mut stream, false),
        (200, 16, b"hello, io_uring\n".to_vec())
    );
    server.finish();
}

#[test]
fn http10_uses_matching_response_version_and_closes_by_default() {
    let mut server = Server::start(1, 1000);
    let mut stream = BufReader::new(server.connect());
    stream
        .get_mut()
        .write_all(b"GET / HTTP/1.0\r\n\r\n")
        .unwrap();
    assert_eq!(
        response(&mut stream, false),
        (200, 16, b"hello, io_uring\n".to_vec())
    );
    let mut extra = [0];
    assert_eq!(stream.read(&mut extra).unwrap(), 0);
    server.finish();
}

#[test]
fn real_http_rejects_symlinks_traversal_and_unsupported_methods() {
    let mut server = Server::start(3, 2000);
    let mut stream = BufReader::new(server.connect());
    for (method, path, expected) in [
        ("GET", "/link", 403),
        ("GET", "/%2e%2e/secret", 403),
        ("POST", "/", 405),
    ] {
        write!(
            stream.get_mut(),
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n"
        )
        .unwrap();
        assert_eq!(response(&mut stream, false), (expected, 0, vec![]));
    }
    server.finish();
}

#[test]
fn real_http_large_file_and_request_body_drain() {
    let mut server = Server::start(2, 2000);
    let mut stream = BufReader::new(server.connect());
    stream.get_mut().write_all(b"GET /large HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\nbodyGET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
    let (status, length, body) = response(&mut stream, false);
    assert_eq!((status, length), (200, 512 * 1024));
    assert!(body.iter().all(|byte| *byte == b'x'));
    assert_eq!(response(&mut stream, false).0, 200);
    server.finish();
}

#[test]
fn real_http_chunked_body_extensions_and_trailers_preserve_pipeline() {
    let mut server = Server::start(2, 1000);
    let mut stream = BufReader::new(server.connect());
    for byte in b"GET / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nTrailer: X-End\r\n\r\n3;part=first\r\none\r\n3\r\ntwo\r\n0\r\nX-End: done\r\n\r\nGET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n" {
        stream.get_mut().write_all(&[*byte]).unwrap();
    }
    assert_eq!(response(&mut stream, false).0, 200);
    assert_eq!(response(&mut stream, false).0, 200);
    server.finish();
}

#[test]
fn continue_response_precedes_request_body_drain_and_final_response() {
    let mut server = Server::start(2, 1000);
    let mut stream = BufReader::new(server.connect());
    stream.get_mut().write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\nExpect: 100-continue\r\n\r\n").unwrap();
    let mut line = String::new();
    stream.read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 100 "));
    loop {
        line.clear();
        stream.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        assert!(!line.is_empty());
    }
    stream
        .get_mut()
        .write_all(b"bodyGET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    assert_eq!(response(&mut stream, false).0, 200);
    assert_eq!(response(&mut stream, false).0, 200);
    server.finish();
}

#[test]
fn slow_request_times_out_without_blocking_a_sibling() {
    let mut server = Server::start(2, 150);
    let mut slow = server.connect();
    slow.write_all(b"GET / HTTP/1.1\r\nHost:").unwrap();
    let mut fast = BufReader::new(server.connect());
    fast.get_mut()
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    assert_eq!(response(&mut fast, false).0, 200);
    let mut slow = BufReader::new(slow);
    assert_eq!(response(&mut slow, false), (408, 0, Vec::new()));
    let mut byte = [0];
    assert_eq!(slow.read(&mut byte).unwrap(), 0);
    let mut second = BufReader::new(server.connect());
    second
        .get_mut()
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    assert_eq!(response(&mut second, false).0, 200);
    server.finish();
}

#[test]
fn stalled_response_writer_times_out_and_settles_its_body_lease() {
    let mut server = Server::start(1, 100);
    std::fs::File::create(server.root.join("huge"))
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let mut slow = server.connect();
    rustix::net::sockopt::set_socket_recv_buffer_size(&slow, 4096).unwrap();
    slow.write_all(b"GET /huge HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    // The peer keeps the connection open but consumes no response bytes.
    server.finish();
}

#[test]
fn disconnect_during_a_large_body_does_not_block_other_connections() {
    let mut server = Server::start(2, 1000);
    std::fs::File::create(server.root.join("huge"))
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let mut abandoned = server.connect();
    abandoned
        .write_all(b"GET /huge HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut prefix = [0; 32];
    abandoned.read_exact(&mut prefix).unwrap();
    drop(abandoned);
    let mut active = BufReader::new(server.connect());
    active
        .get_mut()
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    assert_eq!(response(&mut active, false).0, 200);
    server.finish();
}

#[test]
fn truncation_after_http_headers_closes_without_false_body_completion() {
    let mut server = Server::start(1, 1000);
    let path = server.root.join("huge");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let mut stream = BufReader::new(server.connect());
    stream
        .get_mut()
        .write_all(b"GET /huge HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut line = String::new();
    stream.read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 200 "));
    loop {
        line.clear();
        stream.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        assert!(!line.is_empty());
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(0)
        .unwrap();
    let mut received = 0;
    let mut chunk = [0; 8192];
    loop {
        let count = stream.read(&mut chunk).unwrap();
        if count == 0 {
            break;
        }
        received += count;
    }
    assert!(received < 64 * 1024 * 1024);
    server.finish();
}

#[test]
fn expiring_a_stalled_response_preserves_an_active_sibling() {
    let mut server = Server::start(8, 300);
    std::fs::File::create(server.root.join("huge"))
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let mut slow = server.connect();
    rustix::net::sockopt::set_socket_recv_buffer_size(&slow, 4096).unwrap();
    slow.write_all(b"GET /huge HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut active = BufReader::new(server.connect());
    for _ in 0..7 {
        active
            .get_mut()
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        assert_eq!(
            response(&mut active, false),
            (200, 16, b"hello, io_uring\n".to_vec())
        );
        std::thread::sleep(Duration::from_millis(60));
    }
    server.finish();
}

#[test]
fn malformed_request_gets_one_core_error_response_then_close() {
    let mut server = Server::start(1, 1000);
    let mut rejected = BufReader::new(server.connect());
    rejected.get_mut().write_all(
        b"GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nGET / HTTP/1.1\r\nHost: localhost\r\n\r\n"
    ).unwrap();
    assert_eq!(response(&mut rejected, false), (400, 0, Vec::new()));
    let mut extra = [0];
    match rejected.read(&mut extra) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        result => panic!("unexpected bytes after rejection: {result:?}"),
    }
    let mut active = BufReader::new(server.connect());
    active
        .get_mut()
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    assert_eq!(response(&mut active, false).0, 200);
    server.finish();
}
