// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{
    io::{BufRead, BufReader, Read},
    net::SocketAddr,
    num::NonZeroUsize,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use bytes::Bytes;
use http::Request;
use kimojio_stack::{Errno, IoFd, Runtime, RuntimeContext};
use kimojio_stack_grpc::{Metadata, RuntimeUnaryClient, UnaryResponse};
use kimojio_stack_http::{Body, HttpConfig, RuntimeStackTransport, StackTransport, h2, http1};
use kimojio_stack_steal::{
    RingFd, Runtime as StealRuntime, RuntimeConfig as StealRuntimeConfig,
    RuntimeContext as StealContext, SchedulerConfig, TenantId,
};
use rustix::net::{AddressFamily, SocketFlags, SocketType, ipproto, socketpair};

use super::{
    admin::AdminStatus,
    model::StorageFailureClass,
    proto,
    stack_steal::{StackGrpcRuntime, StackStealGrpcRuntime},
    stackful::{
        StackfulGatewayClient, StackfulGatewayConfig, StackfulGatewayState, serve_admin_once,
        serve_grpc_once, serve_grpc_requests,
    },
    telemetry::HermeticTelemetrySink,
};

/// Runtime implementation exercised by local conformance launches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalRuntime {
    Stack,
    StackSteal,
    StackStealSfq,
}

impl LocalRuntime {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stack => "stack",
            Self::StackSteal => "stack-steal",
            Self::StackStealSfq => "stack-steal-sfq",
        }
    }
}

/// Host runtime selected when launching the standalone stackful gateway binary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostRuntime {
    Stack,
    StackSteal,
}

impl HostRuntime {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stack => "stack",
            Self::StackSteal => "stack-steal",
        }
    }

    const fn host_arg(self) -> &'static str {
        match self {
            Self::Stack => "stack",
            Self::StackSteal => "steal",
        }
    }
}

/// Configuration for a bounded standalone host readiness launch.
#[derive(Clone, Debug)]
pub struct HostProcessConfig {
    pub binary: PathBuf,
    pub runtime: HostRuntime,
    pub workers: NonZeroUsize,
    pub request_deadline: Duration,
}

impl HostProcessConfig {
    pub fn new(binary: impl Into<PathBuf>, runtime: HostRuntime) -> Self {
        Self {
            binary: binary.into(),
            runtime,
            workers: NonZeroUsize::new(2).expect("non-zero default host workers"),
            request_deadline: Duration::from_secs(1),
        }
    }
}

/// Parsed readiness details emitted by a bounded standalone host launch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostReady {
    pub runtime: HostRuntime,
    pub grpc_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    pub stdout: String,
    pub stderr: String,
}

/// Launches the standalone host, waits for its built-in readiness self-check, and returns
/// the addresses it bound. The host exits on its own because `--shutdown-after-ready` is used.
pub fn run_host_ready_check(config: &HostProcessConfig) -> Result<HostReady> {
    let output = Command::new(&config.binary)
        .arg("--runtime")
        .arg(config.runtime.host_arg())
        .arg("--grpc-addr")
        .arg("127.0.0.1:0")
        .arg("--admin-addr")
        .arg("127.0.0.1:0")
        .arg("--workers")
        .arg(config.workers.get().to_string())
        .arg("--request-deadline-ms")
        .arg(config.request_deadline.as_millis().to_string())
        .arg("--shutdown-after-ready")
        .output()
        .with_context(|| format!("failed to launch {}", config.binary.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    ensure!(
        output.status.success(),
        "host readiness check failed with status {:?}: stdout={stdout} stderr={stderr}",
        output.status.code()
    );
    let startup = stdout
        .lines()
        .find(|line| line.starts_with("object-gateway-stack-host "))
        .context("host did not emit startup readiness line")?;
    Ok(HostReady {
        runtime: config.runtime,
        grpc_addr: parse_socket_field(startup, "grpc_addr")?,
        admin_addr: parse_socket_field(startup, "admin_addr")?,
        stdout,
        stderr,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GoHostConfig {
    pub max_object_bytes: u64,
    pub max_chunk_bytes: usize,
    pub request_deadline_ms: u64,
}

impl Default for GoHostConfig {
    fn default() -> Self {
        Self {
            max_object_bytes: 4 * 1024 * 1024,
            max_chunk_bytes: 64 * 1024,
            request_deadline_ms: 30_000,
        }
    }
}

#[derive(Debug)]
pub struct GoHostProcess {
    child: Child,
    pub grpc_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    pub startup_line: String,
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    const fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> Result<&mut Child> {
        self.child
            .as_mut()
            .context("Go host child process already transferred")
    }

    fn wait(&mut self) -> Result<std::process::ExitStatus> {
        self.child_mut()?
            .wait()
            .context("failed to wait for Go host")
    }

    fn kill_and_wait(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn into_child(mut self) -> Result<Child> {
        self.child
            .take()
            .context("Go host child process already transferred")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.kill_and_wait();
        }
    }
}

impl GoHostProcess {
    pub fn gateway(&self) -> TcpGateway {
        TcpGateway::new(self.grpc_addr, self.admin_addr)
    }
}

impl Drop for GoHostProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn go_object_gateway_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("go/object_gateway")
}

pub fn launch_go_host(config: GoHostConfig) -> Result<GoHostProcess> {
    let child = Command::new("go")
        .arg("run")
        .arg(".")
        .arg("--grpc-addr")
        .arg("127.0.0.1:0")
        .arg("--admin-addr")
        .arg("127.0.0.1:0")
        .arg("--max-object-bytes")
        .arg(config.max_object_bytes.to_string())
        .arg("--max-chunk-bytes")
        .arg(config.max_chunk_bytes.to_string())
        .arg("--request-deadline-ms")
        .arg(config.request_deadline_ms.to_string())
        .current_dir(go_object_gateway_dir())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to launch Go object gateway host")?;
    let mut child = ChildGuard::new(child);
    let stdout = child
        .child_mut()?
        .stdout
        .take()
        .context("Go host stdout was not piped")?;
    let mut stderr = child.child_mut()?.stderr.take();
    let (line_tx, line_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut stdout = BufReader::new(stdout);
        let mut startup_line = String::new();
        let result = stdout
            .read_line(&mut startup_line)
            .map(|read| (read, startup_line));
        let _ = line_tx.send(result);
    });
    let (read, startup_line) = match line_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(result) => result.context("failed to read Go host readiness line")?,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            child.kill_and_wait();
            bail!("timed out waiting for Go host readiness line");
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("Go host readiness reader exited without a result");
        }
    };
    if read == 0 {
        let status = child.wait()?;
        let mut stderr_text = String::new();
        if let Some(mut stderr) = stderr.take() {
            let _ = stderr.read_to_string(&mut stderr_text);
        }
        anyhow::bail!("Go host exited before readiness line with {status}: {stderr_text}");
    }
    ensure!(
        startup_line.starts_with("object-gateway-go-host "),
        "unexpected Go host readiness line: {startup_line:?}"
    );
    let grpc_addr = parse_socket_field(&startup_line, "grpc_addr")?;
    let admin_addr = parse_socket_field(&startup_line, "admin_addr")?;
    Ok(GoHostProcess {
        child: child.into_child()?,
        grpc_addr,
        admin_addr,
        startup_line,
    })
}

/// Protocol target for an already running gateway reachable over TCP.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TcpGateway {
    grpc_addr: SocketAddr,
    admin_addr: SocketAddr,
    request_timeout: Option<Duration>,
}

impl TcpGateway {
    pub const fn new(grpc_addr: SocketAddr, admin_addr: SocketAddr) -> Self {
        Self {
            grpc_addr,
            admin_addr,
            request_timeout: None,
        }
    }

    pub const fn with_request_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub const fn grpc_addr(&self) -> SocketAddr {
        self.grpc_addr
    }

    pub const fn admin_addr(&self) -> SocketAddr {
        self.admin_addr
    }

    pub fn put(
        &self,
        namespace: &str,
        object: &str,
        chunks: Vec<Bytes>,
    ) -> Result<proto::PutResponse> {
        self.grpc_rpc(|cx, client| Ok(client.put(cx, namespace, object, chunks)?.message))
    }

    pub fn get(
        &self,
        namespace: &str,
        object: &str,
        max_chunk_bytes: u32,
    ) -> Result<Vec<proto::GetChunk>> {
        self.grpc_rpc(|cx, client| Ok(client.get(cx, namespace, object, max_chunk_bytes)?))
    }

    pub fn delete(&self, namespace: &str, object: &str) -> Result<proto::DeleteResponse> {
        let request = proto::DeleteRequest {
            object: Some(identity(namespace, object)),
        };
        self.grpc_rpc(|cx, client| {
            let response: UnaryResponse<proto::DeleteResponse> =
                client.unary(cx, proto::DELETE_PATH, &request)?;
            Ok(response.message)
        })
    }

    pub fn list(&self, namespace: &str, prefix: &str) -> Result<proto::ListResponse> {
        let request = proto::ListRequest {
            namespace: namespace.to_owned(),
            prefix: prefix.to_owned(),
            max_results: 0,
        };
        self.grpc_rpc(|cx, client| {
            let response: UnaryResponse<proto::ListResponse> =
                client.unary(cx, proto::LIST_PATH, &request)?;
            Ok(response.message)
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
        self.grpc_rpc(|cx, client| {
            let response: UnaryResponse<proto::CopyResponse> =
                client.unary(cx, proto::COPY_PATH, &request)?;
            Ok(response.message)
        })
    }

    pub fn grpc_health(&self) -> Result<proto::HealthResponse> {
        self.grpc_rpc(|cx, client| {
            let response: UnaryResponse<proto::HealthResponse> =
                client.unary(cx, proto::HEALTH_PATH, &proto::HealthRequest {})?;
            Ok(response.message)
        })
    }

    pub fn steady_small_object(
        &self,
        namespace: &str,
        iterations: usize,
        max_chunk_bytes: u32,
    ) -> Result<Vec<(proto::ObjectStatusCode, Duration)>> {
        self.grpc_rpc(|cx, client| {
            let mut samples = Vec::with_capacity(iterations.saturating_mul(2));
            for index in 0..iterations {
                let object = format!("steady-small-{index}");
                let body = Bytes::from(format!("steady-object-{index}"));
                let started = Instant::now();
                let put = client.put(cx, namespace, &object, vec![body.clone()])?;
                samples.push((status_code(put.message.status.as_ref()), started.elapsed()));

                let started = Instant::now();
                let get = client.get(cx, namespace, &object, max_chunk_bytes)?;
                let elapsed = started.elapsed();
                ensure!(
                    get.iter()
                        .flat_map(|chunk| chunk.data.iter().copied())
                        .collect::<Vec<_>>()
                        == body,
                    "steady small-object workload body mismatch for {object}"
                );
                samples.push((get_status(&get), elapsed));
            }
            Ok(samples)
        })
    }

    pub fn admin_health(&self) -> Result<String> {
        self.admin_request("/health")
    }

    pub fn admin_status(&self) -> Result<String> {
        self.admin_request("/status")
    }

    pub fn cancel_get_after_first_chunk(&self, namespace: &str, object: &str) -> Result<()> {
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            let mut transport = stack_tcp_connect(cx, self.grpc_addr)?;
            transport.set_io_timeout(self.request_timeout);
            let http = h2::ClientConnection::new(transport, HttpConfig::default());
            let mut client =
                RuntimeUnaryClient::new(http, kimojio_stack_grpc::ClientConfig::default());
            let request = proto::GetRequest {
                object: Some(identity(namespace, object)),
                max_chunk_bytes: 1,
            };
            let mut stream = client.call_server_streaming::<_, proto::GetChunk>(
                cx,
                proto::GET_PATH,
                Metadata::new(),
                &request,
            )?;
            ensure!(
                stream.next(cx)?.is_some(),
                "GET stream ended before cancellation"
            );
            stream.cancel(cx)?;
            client.close(cx)?;
            Ok(())
        })
    }

    pub fn idle_grpc_request_times_out(&self) -> Result<()> {
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            let mut transport = stack_tcp_connect(cx, self.grpc_addr)?;
            transport.set_io_timeout(self.request_timeout);
            let http = h2::ClientConnection::new(transport, HttpConfig::default());
            let mut client =
                RuntimeUnaryClient::new(http, kimojio_stack_grpc::ClientConfig::default());
            let mut sent = 0usize;
            let requests = std::iter::from_fn(move || {
                let chunk = match sent {
                    0 => Some(proto::PutChunk {
                        object: Some(identity("deadline", "slow-put")),
                        offset: 0,
                        data: Bytes::from_static(b"ab"),
                        finish: false,
                        content_type: "application/octet-stream".to_owned(),
                        user_metadata: Default::default(),
                    }),
                    1 => {
                        thread::sleep(Duration::from_millis(150));
                        Some(proto::PutChunk {
                            object: Some(identity("deadline", "slow-put")),
                            offset: 2,
                            data: Bytes::from_static(b"cd"),
                            finish: true,
                            content_type: "application/octet-stream".to_owned(),
                            user_metadata: Default::default(),
                        })
                    }
                    _ => None,
                };
                sent += 1;
                chunk
            });
            let result: Result<UnaryResponse<proto::PutResponse>, _> =
                client.call_client_streaming(cx, proto::PUT_PATH, Metadata::new(), requests);
            let _ = client.close(cx);
            ensure!(
                result
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.to_string().contains("deadline")),
                "expected slow client-streaming PUT to hit request deadline, got {result:?}"
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
                .map_err(|_| anyhow!("first concurrent TCP PUT panicked"))??;
            let second = second_handle
                .join()
                .map_err(|_| anyhow!("second concurrent TCP PUT panicked"))??;
            Ok((first, second))
        })
    }

    fn grpc_rpc<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&RuntimeContext<'_>, &mut StackfulGatewayClient<IoFd>) -> Result<T>,
    {
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            let mut transport = stack_tcp_connect(cx, self.grpc_addr)?;
            transport.set_io_timeout(self.request_timeout);
            let http = h2::ClientConnection::new(transport, HttpConfig::default());
            let mut client = StackfulGatewayClient::new(RuntimeUnaryClient::new(
                http,
                kimojio_stack_grpc::ClientConfig::default(),
            ));
            let result = f(cx, &mut client);
            let close = client.close(cx).context("target gRPC client close failed");
            match (result, close) {
                (Ok(value), Ok(())) => Ok(value),
                (Err(error), _) => Err(error),
                (Ok(_), Err(error)) => Err(error),
            }
        })
    }

    fn admin_request(&self, path: &'static str) -> Result<String> {
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            let mut transport = stack_tcp_connect(cx, self.admin_addr)?;
            transport.set_io_timeout(self.request_timeout);
            http1_request(cx, transport, path)
        })
    }
}

/// In-process gateway launch target used by hermetic conformance tests.
#[derive(Clone, Debug)]
pub struct LocalGateway {
    runtime: LocalRuntime,
    state: Arc<StackfulGatewayState>,
    steal_workers: NonZeroUsize,
    steal_scheduler: SchedulerConfig,
}

impl LocalGateway {
    pub fn new(runtime: LocalRuntime, config: StackfulGatewayConfig) -> Self {
        Self {
            runtime,
            state: StackfulGatewayState::new(config),
            steal_workers: NonZeroUsize::new(2).expect("non-zero default steal workers"),
            steal_scheduler: SchedulerConfig::default(),
        }
    }

    pub fn new_with_steal_scheduler(
        runtime: LocalRuntime,
        config: StackfulGatewayConfig,
        steal_scheduler: SchedulerConfig,
    ) -> Self {
        Self {
            runtime,
            state: StackfulGatewayState::new(config),
            steal_workers: NonZeroUsize::new(2).expect("non-zero default steal workers"),
            steal_scheduler,
        }
    }

    pub fn runtime(&self) -> LocalRuntime {
        self.runtime
    }

    pub fn state(&self) -> Arc<StackfulGatewayState> {
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

    fn steal_runtime_config(&self) -> StealRuntimeConfig {
        let mut config = StealRuntimeConfig {
            workers: self.steal_workers,
            ..StealRuntimeConfig::default()
        };
        config.scheduler = self.steal_scheduler;
        config
    }

    pub fn put(
        &self,
        namespace: &str,
        object: &str,
        chunks: Vec<Bytes>,
    ) -> Result<proto::PutResponse> {
        match self.runtime {
            LocalRuntime::Stack => self.stack_rpc(1, |cx, client| {
                Ok(client.put(cx, namespace, object, chunks)?.message)
            }),
            LocalRuntime::StackSteal | LocalRuntime::StackStealSfq => self
                .steal_rpc(1, |cx, client| {
                    Ok(client.put(cx, namespace, object, chunks)?.message)
                }),
        }
    }

    pub fn get(
        &self,
        namespace: &str,
        object: &str,
        max_chunk_bytes: u32,
    ) -> Result<Vec<proto::GetChunk>> {
        match self.runtime {
            LocalRuntime::Stack => self.stack_rpc(1, |cx, client| {
                Ok(client.get(cx, namespace, object, max_chunk_bytes)?)
            }),
            LocalRuntime::StackSteal | LocalRuntime::StackStealSfq => self
                .steal_rpc(1, |cx, client| {
                    Ok(client.get(cx, namespace, object, max_chunk_bytes)?)
                }),
        }
    }

    pub fn delete(&self, namespace: &str, object: &str) -> Result<proto::DeleteResponse> {
        let request = proto::DeleteRequest {
            object: Some(identity(namespace, object)),
        };
        match self.runtime {
            LocalRuntime::Stack => self.stack_rpc(1, |cx, client| {
                let response: UnaryResponse<proto::DeleteResponse> =
                    client.unary(cx, proto::DELETE_PATH, &request)?;
                Ok(response.message)
            }),
            LocalRuntime::StackSteal | LocalRuntime::StackStealSfq => {
                self.steal_rpc(1, |cx, client| {
                    let response: UnaryResponse<proto::DeleteResponse> =
                        client.unary(cx, proto::DELETE_PATH, &request)?;
                    Ok(response.message)
                })
            }
        }
    }

    pub fn list(&self, namespace: &str, prefix: &str) -> Result<proto::ListResponse> {
        let request = proto::ListRequest {
            namespace: namespace.to_owned(),
            prefix: prefix.to_owned(),
            max_results: 0,
        };
        match self.runtime {
            LocalRuntime::Stack => self.stack_rpc(1, |cx, client| {
                let response: UnaryResponse<proto::ListResponse> =
                    client.unary(cx, proto::LIST_PATH, &request)?;
                Ok(response.message)
            }),
            LocalRuntime::StackSteal | LocalRuntime::StackStealSfq => {
                self.steal_rpc(1, |cx, client| {
                    let response: UnaryResponse<proto::ListResponse> =
                        client.unary(cx, proto::LIST_PATH, &request)?;
                    Ok(response.message)
                })
            }
        }
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
        match self.runtime {
            LocalRuntime::Stack => self.stack_rpc(1, |cx, client| {
                let response: UnaryResponse<proto::CopyResponse> =
                    client.unary(cx, proto::COPY_PATH, &request)?;
                Ok(response.message)
            }),
            LocalRuntime::StackSteal | LocalRuntime::StackStealSfq => {
                self.steal_rpc(1, |cx, client| {
                    let response: UnaryResponse<proto::CopyResponse> =
                        client.unary(cx, proto::COPY_PATH, &request)?;
                    Ok(response.message)
                })
            }
        }
    }

    pub fn grpc_health(&self) -> Result<proto::HealthResponse> {
        match self.runtime {
            LocalRuntime::Stack => self.stack_rpc(1, |cx, client| {
                let response: UnaryResponse<proto::HealthResponse> =
                    client.unary(cx, proto::HEALTH_PATH, &proto::HealthRequest {})?;
                Ok(response.message)
            }),
            LocalRuntime::StackSteal | LocalRuntime::StackStealSfq => {
                self.steal_rpc(1, |cx, client| {
                    let response: UnaryResponse<proto::HealthResponse> =
                        client.unary(cx, proto::HEALTH_PATH, &proto::HealthRequest {})?;
                    Ok(response.message)
                })
            }
        }
    }

    pub fn admin_health(&self) -> Result<String> {
        self.admin_request("/health")
    }

    pub fn admin_status(&self) -> Result<String> {
        self.admin_request("/status")
    }

    pub fn cancel_get_after_first_chunk(&self, namespace: &str, object: &str) -> Result<()> {
        match self.runtime {
            LocalRuntime::Stack => self.stack_cancel_get(namespace, object),
            LocalRuntime::StackSteal | LocalRuntime::StackStealSfq => {
                self.steal_cancel_get(namespace, object)
            }
        }
    }

    pub fn idle_grpc_request_times_out(&self) -> Result<()> {
        match self.runtime {
            LocalRuntime::Stack => self.stack_idle_timeout(),
            LocalRuntime::StackSteal | LocalRuntime::StackStealSfq => self.steal_idle_timeout(),
        }
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
            let first = (first.0.to_owned(), first.1);
            let second = (second.0.to_owned(), second.1);
            let first_handle =
                scope.spawn(move || first_gateway.put(namespace, &first.0, vec![first.1]));
            let second_handle =
                scope.spawn(move || second_gateway.put(namespace, &second.0, vec![second.1]));
            let first = first_handle
                .join()
                .map_err(|_| anyhow!("first concurrent PUT panicked"))??;
            let second = second_handle
                .join()
                .map_err(|_| anyhow!("second concurrent PUT panicked"))??;
            Ok((first, second))
        })
    }

    pub fn put_many_concurrently(
        &self,
        puts: Vec<(String, String, Vec<Bytes>, usize)>,
    ) -> Result<Vec<(proto::PutResponse, Duration)>> {
        if puts.is_empty() {
            return Ok(Vec::new());
        }
        match self.runtime {
            LocalRuntime::Stack => self.stack_put_many_concurrently(puts),
            LocalRuntime::StackSteal | LocalRuntime::StackStealSfq => {
                self.steal_put_many_concurrently(puts)
            }
        }
    }

    pub fn admin_status_model(&self) -> AdminStatus {
        self.state.admin_status()
    }

    fn stack_rpc<F, T>(&self, requests: usize, f: F) -> Result<T>
    where
        F: FnOnce(&RuntimeContext<'_>, &mut StackfulGatewayClient<IoFd>) -> Result<T>,
    {
        let (client_transport, server_transport) = stack_transport_pair();
        let state = self.state.clone();
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    serve_grpc_requests::<StackGrpcRuntime, _>(
                        cx,
                        server_transport,
                        state,
                        requests,
                    )
                    .context("stack gRPC server failed")
                });
                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = StackfulGatewayClient::new(RuntimeUnaryClient::new(
                        http,
                        kimojio_stack_grpc::ClientConfig::default(),
                    ));
                    let result = f(cx, &mut client);
                    let close = client.close(cx).context("stack gRPC client close failed");
                    match (result, close) {
                        (Ok(value), Ok(())) => Ok(value),
                        (Err(error), _) => Err(error),
                        (Ok(_), Err(error)) => Err(error),
                    }
                });

                let client_result = client.join(cx);
                let server_result = server.join(cx);
                let value = client_result?;
                server_result?;
                Ok(value)
            })
        })
    }

    fn stack_put_many_concurrently(
        &self,
        puts: Vec<(String, String, Vec<Bytes>, usize)>,
    ) -> Result<Vec<(proto::PutResponse, Duration)>> {
        let state = self.state.clone();
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let mut servers = Vec::with_capacity(puts.len());
                let mut clients = Vec::with_capacity(puts.len());
                for (index, (namespace, object, chunks, _tenant_key)) in
                    puts.into_iter().enumerate()
                {
                    let (client_transport, server_transport) = stack_transport_pair();
                    let state = state.clone();
                    servers.push(scope.spawn(move |cx| {
                        serve_grpc_requests::<StackGrpcRuntime, _>(cx, server_transport, state, 1)
                            .context("stack batch gRPC server failed")
                    }));
                    clients.push(scope.spawn(move |cx| {
                        let http =
                            h2::ClientConnection::new(client_transport, HttpConfig::default());
                        let mut client = StackfulGatewayClient::new(RuntimeUnaryClient::new(
                            http,
                            kimojio_stack_grpc::ClientConfig::default(),
                        ));
                        let started = Instant::now();
                        let result = client
                            .put(cx, &namespace, &object, chunks)
                            .context("stack batch PUT failed");
                        let elapsed = started.elapsed();
                        let close = client
                            .close(cx)
                            .context("stack batch gRPC client close failed");
                        match (result, close) {
                            (Ok(response), Ok(())) => Ok((index, response.message, elapsed)),
                            (Err(error), _) => Err(error),
                            (Ok(_), Err(error)) => Err(error),
                        }
                    }));
                }

                let mut responses = Vec::with_capacity(clients.len());
                responses.resize_with(clients.len(), || None);
                for client in clients {
                    let (index, response, elapsed) = client.join(cx)?;
                    responses[index] = Some((response, elapsed));
                }
                for server in servers {
                    server.join(cx)?;
                }
                responses
                    .into_iter()
                    .map(|response| {
                        response.ok_or_else(|| anyhow!("missing stack batch PUT response"))
                    })
                    .collect()
            })
        })
    }

    fn steal_rpc<F, T>(&self, requests: usize, f: F) -> Result<T>
    where
        F: FnOnce(&StealContext<'_>, &mut StackfulGatewayClient<RingFd>) -> Result<T>,
    {
        let (client_transport, server_transport) = steal_transport_pair();
        let state = self.state.clone();
        let mut runtime = StealRuntime::with_config(self.steal_runtime_config());
        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn_stealable(move |cx| {
                    serve_grpc_requests::<StackStealGrpcRuntime, _>(
                        cx,
                        server_transport,
                        state,
                        requests,
                    )
                    .context("stack-steal gRPC server failed")
                });
                let client = scope.spawn_local(move |cx| {
                    let http =
                        h2::RuntimeClientConnection::new(client_transport, HttpConfig::default());
                    let mut client = StackfulGatewayClient::new(RuntimeUnaryClient::new(
                        http,
                        kimojio_stack_grpc::ClientConfig::default(),
                    ));
                    let result = f(cx, &mut client);
                    let close = client
                        .close(cx)
                        .context("stack-steal gRPC client close failed");
                    match (result, close) {
                        (Ok(value), Ok(())) => Ok(value),
                        (Err(error), _) => Err(error),
                        (Ok(_), Err(error)) => Err(error),
                    }
                });

                let client_result = client.join(cx);
                let server_result = server.join(cx);
                let value = client_result?;
                server_result?;
                Ok(value)
            })
        })
    }

    fn steal_put_many_concurrently(
        &self,
        puts: Vec<(String, String, Vec<Bytes>, usize)>,
    ) -> Result<Vec<(proto::PutResponse, Duration)>> {
        let state = self.state.clone();
        let mut runtime = StealRuntime::with_config(self.steal_runtime_config());
        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let mut tenant_map = Vec::<(usize, TenantId)>::new();
                let mut servers = Vec::with_capacity(puts.len());
                let mut clients = Vec::with_capacity(puts.len());
                for (index, (namespace, object, chunks, tenant_key)) in puts.into_iter().enumerate()
                {
                    let tenant = tenant_for_key(scope, &mut tenant_map, tenant_key);
                    let (client_transport, server_transport) = steal_transport_pair();
                    let state = state.clone();
                    servers.push(scope.spawn_stealable_with_tenant(tenant, move |cx| {
                        serve_grpc_requests::<StackStealGrpcRuntime, _>(
                            cx,
                            server_transport,
                            state,
                            1,
                        )
                        .context("stack-steal batch gRPC server failed")
                    }));
                    clients.push(scope.spawn_local_with_tenant(tenant, move |cx| {
                        let http = h2::RuntimeClientConnection::new(
                            client_transport,
                            HttpConfig::default(),
                        );
                        let mut client = StackfulGatewayClient::new(RuntimeUnaryClient::new(
                            http,
                            kimojio_stack_grpc::ClientConfig::default(),
                        ));
                        let started = Instant::now();
                        let result = client
                            .put(cx, &namespace, &object, chunks)
                            .context("stack-steal batch PUT failed");
                        let elapsed = started.elapsed();
                        let close = client
                            .close(cx)
                            .context("stack-steal batch gRPC client close failed");
                        match (result, close) {
                            (Ok(response), Ok(())) => Ok((index, response.message, elapsed)),
                            (Err(error), _) => Err(error),
                            (Ok(_), Err(error)) => Err(error),
                        }
                    }));
                }

                let mut responses = Vec::with_capacity(clients.len());
                responses.resize_with(clients.len(), || None);
                for client in clients {
                    let (index, response, elapsed) = client.join(cx)?;
                    responses[index] = Some((response, elapsed));
                }
                for server in servers {
                    server.join(cx)?;
                }
                responses
                    .into_iter()
                    .map(|response| {
                        response.ok_or_else(|| anyhow!("missing stack-steal batch PUT response"))
                    })
                    .collect()
            })
        })
    }

    fn admin_request(&self, path: &'static str) -> Result<String> {
        match self.runtime {
            LocalRuntime::Stack => self.stack_admin_request(path),
            LocalRuntime::StackSteal | LocalRuntime::StackStealSfq => {
                self.steal_admin_request(path)
            }
        }
    }

    fn stack_admin_request(&self, path: &'static str) -> Result<String> {
        let (client_transport, server_transport) = stack_transport_pair();
        let state = self.state.clone();
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    serve_admin_once(cx, server_transport, state).context("stack admin failed")
                });
                let client = scope.spawn(move |cx| http1_request(cx, client_transport, path));
                let response = client.join(cx)?;
                server.join(cx)?;
                Ok(response)
            })
        })
    }

    fn steal_admin_request(&self, path: &'static str) -> Result<String> {
        let (client_transport, server_transport) = steal_transport_pair();
        let state = self.state.clone();
        let mut runtime = StealRuntime::with_config(self.steal_runtime_config());
        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn_stealable(move |cx| {
                    serve_admin_once(cx, server_transport, state)
                        .context("stack-steal admin failed")
                });
                let client = scope.spawn_local(move |cx| http1_request(cx, client_transport, path));
                let response = client.join(cx)?;
                server.join(cx)?;
                Ok(response)
            })
        })
    }

    fn stack_cancel_get(&self, namespace: &str, object: &str) -> Result<()> {
        let (client_transport, server_transport) = stack_transport_pair();
        let state = self.state.clone();
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    serve_grpc_once::<StackGrpcRuntime, _>(cx, server_transport, state)
                });
                let request = proto::GetRequest {
                    object: Some(identity(namespace, object)),
                    max_chunk_bytes: 1,
                };
                let client = scope.spawn(move |cx| {
                    let http = h2::ClientConnection::new(client_transport, HttpConfig::default());
                    let mut client =
                        RuntimeUnaryClient::new(http, kimojio_stack_grpc::ClientConfig::default());
                    let mut stream = client.call_server_streaming::<_, proto::GetChunk>(
                        cx,
                        proto::GET_PATH,
                        Metadata::new(),
                        &request,
                    )?;
                    ensure!(
                        stream.next(cx)?.is_some(),
                        "GET stream ended before cancellation"
                    );
                    stream.cancel(cx)?;
                    client.close(cx)?;
                    Ok::<_, anyhow::Error>(())
                });
                client.join(cx)?;
                let _ = server.join(cx);
                Ok(())
            })
        })
    }

    fn steal_cancel_get(&self, namespace: &str, object: &str) -> Result<()> {
        let (client_transport, server_transport) = steal_transport_pair();
        let state = self.state.clone();
        let mut runtime = StealRuntime::with_config(self.steal_runtime_config());
        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn_stealable(move |cx| {
                    serve_grpc_once::<StackStealGrpcRuntime, _>(cx, server_transport, state)
                });
                let request = proto::GetRequest {
                    object: Some(identity(namespace, object)),
                    max_chunk_bytes: 1,
                };
                let client = scope.spawn_local(move |cx| {
                    let http =
                        h2::RuntimeClientConnection::new(client_transport, HttpConfig::default());
                    let mut client =
                        RuntimeUnaryClient::new(http, kimojio_stack_grpc::ClientConfig::default());
                    let mut stream = client.call_server_streaming::<_, proto::GetChunk>(
                        cx,
                        proto::GET_PATH,
                        Metadata::new(),
                        &request,
                    )?;
                    ensure!(
                        stream.next(cx)?.is_some(),
                        "GET stream ended before cancellation"
                    );
                    stream.cancel(cx)?;
                    client.close(cx)?;
                    Ok::<_, anyhow::Error>(())
                });
                client.join(cx)?;
                let _ = server.join(cx);
                Ok(())
            })
        })
    }

    fn stack_idle_timeout(&self) -> Result<()> {
        let (client_transport, server_transport) = stack_transport_pair();
        let state = self.state.clone();
        let mut runtime = Runtime::new();
        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn(move |cx| {
                    serve_grpc_once::<StackGrpcRuntime, _>(cx, server_transport, state)
                });
                let client = scope.spawn(move |cx| {
                    cx.sleep(std::time::Duration::from_millis(100))
                        .context("sleep failed")?;
                    drop(client_transport);
                    Ok::<_, anyhow::Error>(())
                });
                client.join(cx)?;
                let result = server.join(cx);
                ensure!(
                    grpc_error_is_timeout(&result),
                    "expected timeout, got {result:?}"
                );
                Ok(())
            })
        })
    }

    fn steal_idle_timeout(&self) -> Result<()> {
        let (client_transport, server_transport) = steal_transport_pair();
        let state = self.state.clone();
        let mut runtime = StealRuntime::with_config(self.steal_runtime_config());
        runtime.block_on(|cx| {
            cx.scope(|scope| {
                let server = scope.spawn_stealable(move |cx| {
                    serve_grpc_once::<StackStealGrpcRuntime, _>(cx, server_transport, state)
                });
                let client = scope.spawn_local(move |cx| {
                    cx.sleep(std::time::Duration::from_millis(100))
                        .context("sleep failed")?;
                    drop(client_transport);
                    Ok::<_, anyhow::Error>(())
                });
                client.join(cx)?;
                let result = server.join(cx);
                ensure!(
                    grpc_error_is_timeout(&result),
                    "expected timeout, got {result:?}"
                );
                Ok(())
            })
        })
    }
}

fn tenant_for_key(
    scope: &kimojio_stack_steal::Scope<'_, '_>,
    tenant_map: &mut Vec<(usize, TenantId)>,
    key: usize,
) -> TenantId {
    if let Some((_, tenant)) = tenant_map.iter().find(|(candidate, _)| *candidate == key) {
        return *tenant;
    }
    let tenant = scope.new_tenant_id();
    tenant_map.push((key, tenant));
    tenant
}

fn http1_request<S>(
    cx: &impl kimojio_stack_http::HttpRuntime<S>,
    transport: RuntimeStackTransport<S>,
    path: &str,
) -> Result<String>
where
    S: kimojio_stack::RuntimeSocket,
{
    let mut client = http1::RuntimeClientConnection::new(transport, HttpConfig::default());
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "127.0.0.1")
        .body(Body::empty())
        .context("failed to build admin request")?;
    let response = client.send(cx, &request)?;
    client.close(cx)?;
    ensure!(
        response.status().is_success(),
        "admin request {path} failed with {}",
        response.status()
    );
    Ok(String::from_utf8_lossy(response.body().as_bytes()).into_owned())
}

fn stack_transport_pair() -> (StackTransport, StackTransport) {
    let (client_fd, server_fd) = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .expect("socketpair should be available");
    (
        StackTransport::plaintext(client_fd),
        StackTransport::plaintext(server_fd),
    )
}

fn steal_transport_pair() -> (RuntimeStackTransport<RingFd>, RuntimeStackTransport<RingFd>) {
    let (client_fd, server_fd) = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .expect("socketpair should be available");
    (
        RuntimeStackTransport::plaintext_socket(RingFd::from_owned(client_fd)),
        RuntimeStackTransport::plaintext_socket(RingFd::from_owned(server_fd)),
    )
}

fn grpc_error_is_timeout(result: &Result<(), kimojio_stack_grpc::Error>) -> bool {
    matches!(
        result,
        Err(kimojio_stack_grpc::Error::Transport(
            kimojio_stack_http::Error::Io(Errno::TIME)
        ))
    )
}

fn parse_socket_field(line: &str, key: &str) -> Result<SocketAddr> {
    let prefix = format!("{key}=");
    let value = line
        .split_whitespace()
        .find_map(|part| part.strip_prefix(&prefix))
        .with_context(|| format!("host startup line missing {key}: {line}"))?;
    value
        .parse()
        .with_context(|| format!("host startup line had invalid {key}: {value}"))
}

fn stack_tcp_connect(cx: &RuntimeContext<'_>, addr: SocketAddr) -> Result<StackTransport> {
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

fn status_code(status: Option<&proto::OperationStatus>) -> proto::ObjectStatusCode {
    status
        .and_then(|status| proto::ObjectStatusCode::try_from(status.code).ok())
        .unwrap_or(proto::ObjectStatusCode::Internal)
}

fn get_status(chunks: &[proto::GetChunk]) -> proto::ObjectStatusCode {
    chunks
        .first()
        .and_then(|chunk| chunk.status.as_ref())
        .map_or(proto::ObjectStatusCode::Internal, |status| {
            status_code(Some(status))
        })
}

fn identity(namespace: &str, object: &str) -> proto::ObjectIdentity {
    proto::ObjectIdentity {
        namespace: namespace.to_owned(),
        name: object.to_owned(),
    }
}
