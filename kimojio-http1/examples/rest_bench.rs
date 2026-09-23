//! Single-fixture TCP comparison server; see interop/rest-bench/README.md.
use kimojio::{OwnedFd, operations, task_pool::TaskPool};
use kimojio_http1::{
    Config, ConnectionId, Error, IncomingBody, OutgoingBody,
    http::{HeaderName, HeaderValue, Request, Response, StatusCode},
    serve_connection_native,
};
use std::{io::Write, net::TcpListener, rc::Rc};

const REQUEST: &[u8] = include_bytes!("../../interop/rest-bench/fixtures/request.json");
const RESPONSE: &[u8] = include_bytes!("../../interop/rest-bench/fixtures/response.json");
const MANIFEST: &str = include_str!("../../interop/rest-bench/fixtures/manifest.json");

struct Fixture {
    method: String,
    path: String,
    request_headers: Vec<(HeaderName, HeaderValue)>,
    response_headers: Vec<(HeaderName, HeaderValue)>,
    request_length: HeaderValue,
    response: Rc<[u8]>,
}
impl Fixture {
    fn new() -> Self {
        let manifest: serde_json::Value = serde_json::from_str(MANIFEST).unwrap();
        let headers = |key: &str| {
            manifest[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|pair| {
                    (
                        HeaderName::from_bytes(pair[0].as_str().unwrap().as_bytes()).unwrap(),
                        HeaderValue::from_str(pair[1].as_str().unwrap()).unwrap(),
                    )
                })
                .collect()
        };
        assert_eq!(manifest["request_bytes"], REQUEST.len());
        assert_eq!(manifest["response_bytes"], RESPONSE.len());
        Self {
            method: manifest["method"].as_str().unwrap().to_owned(),
            path: manifest["path"].as_str().unwrap().to_owned(),
            request_headers: headers("request_headers"),
            response_headers: headers("response_headers"),
            request_length: HeaderValue::from_str(&REQUEST.len().to_string()).unwrap(),
            response: Rc::from(RESPONSE),
        }
    }
    async fn handle(
        &self,
        mut request: Request<IncomingBody>,
    ) -> Result<Response<OutgoingBody>, Error> {
        let bad = || {
            Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(OutgoingBody::empty())
                .unwrap()
        };
        if request.method().as_str() != self.method
            || request.uri() != self.path.as_str()
            || request.headers().get("content-length") != Some(&self.request_length)
            || request.headers().contains_key("transfer-encoding")
            || self
                .request_headers
                .iter()
                .any(|(name, value)| request.headers().get(name) != Some(value))
        {
            return Ok(bad());
        }
        let body = request.body_mut().collect(REQUEST.len()).await?;
        if body != REQUEST {
            return Ok(bad());
        }
        let mut response = Response::new(OutgoingBody::shared(self.response.clone()));
        for (name, value) in &self.response_headers {
            response.headers_mut().insert(name.clone(), value.clone());
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[kimojio::test]
    async fn fixture_validates_request_and_returns_the_shared_response() {
        for corrupt in [false, true] {
            let fixture = Rc::new(Fixture::new());
            let (client_fd, server_fd) = rustix::net::socketpair(
                rustix::net::AddressFamily::UNIX,
                rustix::net::SocketType::STREAM,
                rustix::net::SocketFlags::CLOEXEC,
                None,
            )
            .unwrap();
            let (mut client, driver) = kimojio_http1::connect_native(
                client_fd,
                Config::new(ConnectionId {
                    slot: 1,
                    generation: 1,
                }),
            );
            let server_fixture = fixture.clone();
            let mut server_config = Config::new(ConnectionId {
                slot: 2,
                generation: 1,
            });
            server_config.coalesce_full_bodies = true;
            let server = serve_connection_native(server_fd, server_config, move |request| {
                let fixture = server_fixture.clone();
                async move { fixture.handle(request).await }
            });
            let app = async move {
                let mut payload = REQUEST.to_vec();
                if corrupt {
                    payload[0] ^= 1;
                }
                let mut request = Request::builder()
                    .method(fixture.method.as_str())
                    .uri(fixture.path.as_str())
                    .body(OutgoingBody::full(payload))
                    .unwrap();
                for (name, value) in &fixture.request_headers {
                    request.headers_mut().insert(name.clone(), value.clone());
                }
                let mut response = client.send(request).await.unwrap();
                assert_eq!(
                    response.status(),
                    if corrupt {
                        StatusCode::BAD_REQUEST
                    } else {
                        StatusCode::OK
                    }
                );
                let body = response.body_mut().collect(RESPONSE.len()).await.unwrap();
                if !corrupt {
                    assert_eq!(body, RESPONSE);
                    for (name, value) in &fixture.response_headers {
                        assert_eq!(response.headers().get(name), Some(value));
                    }
                }
                client.shutdown().await.unwrap();
            };
            let ((), client, server) = futures::join!(app, driver.run(), server);
            client.unwrap();
            server.unwrap();
        }
    }
}

#[kimojio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut bind = "127.0.0.1:0".to_owned();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => bind = args.next().ok_or("missing bind address")?,
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    let fixture = Rc::new(Fixture::new());
    let listener = TcpListener::bind(bind)?;
    println!("LISTEN {}", listener.local_addr()?);
    std::io::stdout().flush()?;
    let listener: OwnedFd = listener.into();
    // Non-binding for the harness's <=32 connections, not a lifetime accept cap.
    let pool = TaskPool::new(128);
    let mut tasks = Vec::new();
    let mut slot = 0u64;
    loop {
        let socket = operations::accept(&listener).await?;
        rustix::net::sockopt::set_tcp_nodelay(&socket, true)?;
        rustix::net::sockopt::set_socket_keepalive(&socket, false)?;
        slot = slot.checked_add(1).ok_or("connection identity exhausted")?;
        let fixture = fixture.clone();
        let task = pool
            .spawn_task(async move {
                let mut config = Config::new(ConnectionId {
                    slot,
                    generation: 1,
                });
                config.coalesce_full_bodies = true;
                config.protocol.max_requests = u64::MAX;
                config.protocol.max_buffer_bytes = 16 * 1024;
                config.protocol.max_head_bytes = 16 * 1024;
                config.protocol.max_headers = 64;
                config.protocol.max_body_bytes = REQUEST.len().max(RESPONSE.len()) as u64;
                config.protocol.head_timeout_ns = Some(5_000_000_000);
                config.protocol.idle_timeout_ns = Some(30_000_000_000);
                config.protocol.body_timeout_ns = None;
                config.protocol.continue_timeout_ns = None;
                let result = serve_connection_native(socket, config, move |request| {
                    let fixture = fixture.clone();
                    async move { fixture.handle(request).await }
                })
                .await;
                if let Err(error) = result {
                    eprintln!("connection {slot}: {error}");
                }
            })
            .await
            .map_err(|_| "connection admission canceled")?;
        tasks.retain(|task: &operations::TaskHandle<()>| !task.is_complete());
        tasks.push(task);
    }
}
