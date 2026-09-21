#![doc = include_str!("README.md")]

mod detect;
mod probe;
mod replay;

pub use detect::{DetectionClosed, DetectionFailure};
use http1 as h1;
pub use kimojio_fsm_http1 as http1;
pub use probe::Protocol;
use std::time::Duration;

use crate::SendBuffer;

/// The union of client capabilities, with one caller-defined suspension type.
pub trait ClientPorts<B: SendBuffer>:
    h1::ClientPorts<Vec<u8>, B> + crate::Ports<B, Output = <Self as h1::Ports<Vec<u8>, B>>::Output>
{
}

impl<B: SendBuffer, P> ClientPorts<B> for P where
    P: h1::ClientPorts<Vec<u8>, B> + crate::Ports<B, Output = <P as h1::Ports<Vec<u8>, B>>::Output>
{
}

/// The union of server capabilities, with one caller-defined suspension type.
pub trait ServerPorts<B: SendBuffer>:
    h1::ServerPorts<Vec<u8>, B> + crate::Ports<B, Output = <Self as h1::Ports<Vec<u8>, B>>::Output>
{
    /// Protocol detection failed, and its original operations and close settled.
    fn detection_closed(
        &mut self,
        result: DetectionClosed,
    ) -> Option<<Self as crate::Ports<B>>::Output>;
}

enum ClientInner<B: SendBuffer> {
    Http1(Box<h1::Client<Vec<u8>, B>>),
    Http2(Box<crate::Client<B>>),
}

/// A selected client. TLS/ALPN negotiation belongs to the caller.
pub struct Client<B: SendBuffer = Vec<u8>> {
    inner: ClientInner<B>,
}

impl<B: SendBuffer> Client<B> {
    pub fn http1(client: h1::Client<Vec<u8>, B>) -> Self {
        Self {
            inner: ClientInner::Http1(Box::new(client)),
        }
    }

    pub fn http2(client: crate::Client<B>) -> Self {
        Self {
            inner: ClientInner::Http2(Box::new(client)),
        }
    }

    pub fn protocol(&self) -> Protocol {
        match self.inner {
            ClientInner::Http1(_) => Protocol::Http1,
            ClientInner::Http2(_) => Protocol::Http2,
        }
    }

    pub fn http1_mut(&mut self) -> Option<&mut h1::Client<Vec<u8>, B>> {
        match &mut self.inner {
            ClientInner::Http1(client) => Some(client),
            ClientInner::Http2(_) => None,
        }
    }

    /// Observes the selected HTTP/1 child without driving it.
    #[cfg(feature = "http1-metrics")]
    pub fn http1_metrics(&self) -> Option<h1::MetricsSnapshot> {
        match &self.inner {
            ClientInner::Http1(client) => Some(client.metrics()),
            ClientInner::Http2(_) => None,
        }
    }

    pub fn http2_mut(&mut self) -> Option<&mut crate::Client<B>> {
        match &mut self.inner {
            ClientInner::Http1(_) => None,
            ClientInner::Http2(client) => Some(client),
        }
    }

    pub fn next<P: ClientPorts<B>>(
        &mut self,
        ports: &mut P,
    ) -> Option<<P as crate::Ports<B>>::Output> {
        match &mut self.inner {
            ClientInner::Http1(client) => client.next(ports),
            ClientInner::Http2(client) => client.next(ports),
        }
    }

    /// Hard-aborts the selected child while preserving its original-operation joins.
    pub fn abort(&mut self) {
        match &mut self.inner {
            ClientInner::Http1(client) => client.shutdown(h1::ShutdownMode::Abort),
            ClientInner::Http2(client) => client.abort(),
        }
    }
}

enum ServerInner<B: SendBuffer> {
    Http1(Box<h1::Server<Vec<u8>, B>>),
    Http2(Box<crate::Server<B>>),
    Detect(Box<Detecting<B>>),
    Replay1(Box<h1::Server<Vec<u8>, B>>, replay::Prefix),
    Replay2(Box<crate::Server<B>>, replay::Prefix),
    Moving,
}

struct Detecting<B: SendBuffer> {
    detector: detect::Detector,
    http1: Option<Box<h1::Server<Vec<u8>, B>>>,
    http2: Option<Box<crate::Server<B>>>,
}

pub struct DetectionConfig {
    pub http1_connection: h1::ConnectionId,
    pub http1_config: h1::Config,
    pub http1_buffer: Vec<u8>,
    pub http2_config: crate::Config,
    pub timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Http1(h1::CommandError),
    Http2(crate::CommandError),
    TimeRange,
}

fn tick(now: Duration) -> Result<h1::Tick, Error> {
    u64::try_from(now.as_nanos())
        .map(h1::Tick)
        .map_err(|_| Error::TimeRange)
}

/// A selected server. Each child retains its own command and completion types.
pub struct Server<B: SendBuffer = Vec<u8>> {
    inner: ServerInner<B>,
}

impl<B: SendBuffer> Server<B> {
    /// Detects the prior-knowledge preface before either child parses input.
    ///
    /// Both configurations are checked at construction. Child time origins
    /// remain the connection creation time, including time spent in detection.
    pub fn detect(config: DetectionConfig, now: Duration) -> Result<Self, Error> {
        let detector = detect::Detector::new(now, config.timeout).map_err(Error::Http2)?;
        let http1 = h1::Server::with_output_type(
            config.http1_connection,
            config.http1_config,
            config.http1_buffer,
            tick(now)?,
        )
        .map_err(Error::Http1)?;
        let http2 = crate::Server::new(config.http2_config, now).map_err(Error::Http2)?;
        Ok(Self {
            inner: ServerInner::Detect(Box::new(Detecting {
                detector,
                http1: Some(Box::new(http1)),
                http2: Some(Box::new(http2)),
            })),
        })
    }

    pub fn http1(server: h1::Server<Vec<u8>, B>) -> Self {
        Self {
            inner: ServerInner::Http1(Box::new(server)),
        }
    }

    pub fn http2(server: crate::Server<B>) -> Self {
        Self {
            inner: ServerInner::Http2(Box::new(server)),
        }
    }

    pub fn protocol(&self) -> Option<Protocol> {
        match self.inner {
            ServerInner::Http1(_) | ServerInner::Replay1(..) => Some(Protocol::Http1),
            ServerInner::Http2(_) | ServerInner::Replay2(..) => Some(Protocol::Http2),
            ServerInner::Detect(_) => None,
            ServerInner::Moving => unreachable!("synchronous ownership transfer"),
        }
    }

    pub fn http1_mut(&mut self) -> Option<&mut h1::Server<Vec<u8>, B>> {
        match &mut self.inner {
            ServerInner::Http1(server) | ServerInner::Replay1(server, _) => Some(server),
            _ => None,
        }
    }

    /// Observes the selected HTTP/1 child, including during prefix replay.
    #[cfg(feature = "http1-metrics")]
    pub fn http1_metrics(&self) -> Option<h1::MetricsSnapshot> {
        match &self.inner {
            ServerInner::Http1(server) | ServerInner::Replay1(server, _) => Some(server.metrics()),
            _ => None,
        }
    }

    pub fn http2_mut(&mut self) -> Option<&mut crate::Server<B>> {
        match &mut self.inner {
            ServerInner::Http2(server) | ServerInner::Replay2(server, _) => Some(server),
            _ => None,
        }
    }

    pub fn advance_time(&mut self, now: Duration) -> Result<(), Error> {
        match &mut self.inner {
            ServerInner::Http1(server) | ServerInner::Replay1(server, _) => {
                server.observe_time(tick(now)?).map_err(Error::Http1)
            }
            ServerInner::Http2(server) | ServerInner::Replay2(server, _) => {
                server.advance_time(now).map_err(Error::Http2)
            }
            ServerInner::Detect(state) => {
                tick(now)?;
                state.detector.advance_time(now).map_err(Error::Http2)
            }
            ServerInner::Moving => unreachable!("synchronous ownership transfer"),
        }
    }

    /// Stops detection or requests the selected child's graceful shutdown.
    pub fn shutdown(&mut self) -> Result<(), Error> {
        match &mut self.inner {
            ServerInner::Http1(server) | ServerInner::Replay1(server, _) => {
                server.shutdown(h1::ShutdownMode::Graceful);
                Ok(())
            }
            ServerInner::Http2(server) | ServerInner::Replay2(server, _) => {
                server.shutdown().map_err(Error::Http2)
            }
            ServerInner::Detect(state) => {
                state.detector.abort();
                Ok(())
            }
            ServerInner::Moving => unreachable!("synchronous ownership transfer"),
        }
    }

    /// Hard-aborts detection or the selected child without a graceful-shutdown wait.
    pub fn abort(&mut self) {
        match &mut self.inner {
            ServerInner::Http1(server) | ServerInner::Replay1(server, _) => {
                server.shutdown(h1::ShutdownMode::Abort);
            }
            ServerInner::Http2(server) | ServerInner::Replay2(server, _) => server.abort(),
            ServerInner::Detect(state) => state.detector.abort(),
            ServerInner::Moving => unreachable!("synchronous ownership transfer"),
        }
    }

    /// Routes an HTTP/2-port read, including a detection read.
    pub fn complete_read(
        &mut self,
        completion: crate::ReadCompletion,
    ) -> Result<(), crate::Rejected<crate::ReadCompletion>> {
        match &mut self.inner {
            ServerInner::Detect(state) => state.detector.complete_read(completion),
            ServerInner::Http2(server) | ServerInner::Replay2(server, _) => {
                server.complete_read(completion)
            }
            _ => Err(rejected(completion)),
        }
    }

    pub fn complete_wake(
        &mut self,
        completion: crate::WakeCompletion,
    ) -> Result<(), crate::Rejected<crate::WakeCompletion>> {
        match &mut self.inner {
            ServerInner::Detect(state) => state.detector.complete_wake(completion),
            ServerInner::Http2(server) | ServerInner::Replay2(server, _) => {
                server.complete_wake(completion)
            }
            _ => Err(rejected(completion)),
        }
    }

    pub fn complete_cancel(
        &mut self,
        completion: crate::CancelCompletion,
    ) -> Result<(), crate::Rejected<crate::CancelCompletion>> {
        match &mut self.inner {
            ServerInner::Detect(state) => state.detector.complete_cancel(completion),
            ServerInner::Http2(server) | ServerInner::Replay2(server, _) => {
                server.complete_cancel(completion)
            }
            _ => Err(rejected(completion)),
        }
    }

    pub fn complete_close(
        &mut self,
        completion: crate::CloseCompletion,
    ) -> Result<(), crate::Rejected<crate::CloseCompletion>> {
        match &mut self.inner {
            ServerInner::Detect(state) => state.detector.complete_close(completion),
            ServerInner::Http2(server) | ServerInner::Replay2(server, _) => {
                server.complete_close(completion)
            }
            _ => Err(rejected(completion)),
        }
    }

    pub fn next<P: ServerPorts<B>>(
        &mut self,
        ports: &mut P,
    ) -> Option<<P as crate::Ports<B>>::Output> {
        loop {
            match &mut self.inner {
                ServerInner::Http1(server) => return server.next(ports),
                ServerInner::Http2(server) => return server.next(ports),
                ServerInner::Detect(state) => match state.detector.next(ports) {
                    detect::Step::Output(output) => return Some(output),
                    detect::Step::Blocked => return None,
                    detect::Step::Selected(protocol) => {
                        let prefix = replay::Prefix::new(state.detector.prefix());
                        let now = state.detector.now();
                        self.inner = match protocol {
                            Protocol::Http1 => {
                                let mut server = state.http1.take().expect("unused HTTP1 child");
                                server
                                    .observe_time(tick(now).expect("validated composite clock"))
                                    .expect("monotonic detection time");
                                ServerInner::Replay1(server, prefix)
                            }
                            Protocol::Http2 => {
                                let mut server = state.http2.take().expect("unused HTTP2 child");
                                server.advance_time(now).expect("monotonic detection time");
                                ServerInner::Replay2(server, prefix)
                            }
                        };
                    }
                },
                ServerInner::Replay1(server, prefix) => {
                    if prefix.is_empty() {
                        let ServerInner::Replay1(server, _) =
                            std::mem::replace(&mut self.inner, ServerInner::Moving)
                        else {
                            unreachable!()
                        };
                        self.inner = ServerInner::Http1(server);
                        continue;
                    }
                    match server.next(&mut replay::Ports { outer: ports }) {
                        Some(replay::Step::Read1(mut op)) => {
                            let count = prefix.copy_to(op.bytes_mut());
                            server
                                .complete_read(op.complete(Ok(count)))
                                .expect("owned prefix read");
                        }
                        Some(replay::Step::Output(output)) => return Some(output),
                        Some(replay::Step::Read2(_)) => unreachable!(),
                        None => return None,
                    }
                }
                ServerInner::Replay2(server, prefix) => {
                    if prefix.is_empty() {
                        let ServerInner::Replay2(server, _) =
                            std::mem::replace(&mut self.inner, ServerInner::Moving)
                        else {
                            unreachable!()
                        };
                        self.inner = ServerInner::Http2(server);
                        continue;
                    }
                    match server.next(&mut replay::Ports { outer: ports }) {
                        Some(replay::Step::Read2(mut op)) => {
                            let count = prefix.copy_to(op.buffer_mut());
                            server
                                .complete_read(op.complete(crate::ReadOutcome::Read(count)))
                                .expect("owned prefix read");
                        }
                        Some(replay::Step::Output(output)) => return Some(output),
                        Some(replay::Step::Read1(_)) => unreachable!(),
                        None => return None,
                    }
                }
                ServerInner::Moving => unreachable!("synchronous ownership transfer"),
            }
        }
    }
}

fn rejected<T>(value: T) -> crate::Rejected<T> {
    crate::Rejected {
        error: crate::CommandError::InvalidState,
        value,
    }
}
