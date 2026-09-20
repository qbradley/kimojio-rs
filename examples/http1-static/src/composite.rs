//! Synchronous composition: HTTP owns protocol state; App owns file semantics.
use kimojio_fsm_http1 as http;

use crate::app::{self, Buffer};

#[cfg(test)]
mod tests;

pub trait Ports {
    type Output;
    fn read(&mut self, op: http::ReadOp<Buffer>) -> Option<Self::Output>;
    fn write(&mut self, op: http::WriteOp<Buffer>) -> Option<Self::Output>;
    fn readiness(&mut self, op: http::ReadinessOp) -> Option<Self::Output>;
    fn cancel(&mut self, op: http::CancelOp) -> Option<Self::Output>;
    fn close(&mut self, op: http::CloseOp) -> Option<Self::Output>;
    fn open(&mut self, op: app::Open) -> Option<Self::Output>;
    fn stat(&mut self, op: app::Stat) -> Option<Self::Output>;
    fn file_read(&mut self, op: app::Read) -> Option<Self::Output>;
    fn file_close(&mut self, op: app::Close) -> Option<Self::Output>;
    fn deadline_changed(&mut self, deadline: Option<http::Deadline>) -> Option<Self::Output>;
    fn exchange_finished(&mut self) -> Option<Self::Output>;
    fn closed(&mut self, result: http::ConnectionResult) -> Option<Self::Output>;
    fn yield_turn(&mut self) -> Option<Self::Output>;
}

enum Step<O> {
    External(O),
    Wake,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lifecycle {
    Serving,
    WaitingForApp,
    Terminating,
    Closed,
}

#[derive(Clone, Copy)]
enum Transition {
    ReturnRejected,
    ReleaseIncoming,
    Credit,
    Http,
    Respond,
    App,
}

#[derive(Clone, Copy)]
enum Incoming {
    None,
    Receiving(http::ExchangeId),
    Complete(http::ExchangeId),
}

pub struct Service {
    http: http::Server<Buffer>,
    app: app::App<http::ExchangeId>,
    http_ready: bool,
    http_first: bool,
    app_ready: bool,
    lifecycle: Lifecycle,
    release: Option<http::BodyCompletion<Buffer>>,
    credit: Option<(http::ExchangeId, usize)>,
    response: Option<app::Response<http::ExchangeId>>,
    incoming: Incoming,
    returned_body: Option<(http::ExchangeId, Buffer)>,
}

impl Service {
    pub fn new(id: u64, config: http::Config, now: http::Tick) -> Result<Self, http::CommandError> {
        let buffer = vec![0; config.max_buffer_bytes].into_boxed_slice();
        Ok(Self {
            http: http::Server::new(
                http::ConnectionId {
                    slot: id,
                    generation: 1,
                },
                config,
                buffer,
                now,
            )?,
            app: app::App::new(id),
            http_ready: true,
            http_first: true,
            app_ready: false,
            lifecycle: Lifecycle::Serving,
            release: None,
            credit: None,
            response: None,
            incoming: Incoming::None,
            returned_body: None,
        })
    }

    pub fn settled(&self) -> bool {
        self.check();
        self.lifecycle == Lifecycle::Closed && self.app.is_idle()
    }

    fn check(&self) {
        match self.lifecycle {
            Lifecycle::Serving => {}
            Lifecycle::WaitingForApp => {
                debug_assert!(!self.app.is_idle());
                debug_assert!(self.response.is_none());
                debug_assert!(matches!(self.incoming, Incoming::None));
            }
            Lifecycle::Terminating => debug_assert!(self.response.is_none()),
            Lifecycle::Closed => {
                debug_assert!(self.response.is_none());
                debug_assert!(matches!(self.incoming, Incoming::None));
                debug_assert!(self.release.is_none() && self.credit.is_none());
                debug_assert!(self.returned_body.is_none());
            }
        }
    }

    pub fn complete_read(&mut self, completion: http::ReadCompletion<Buffer>) {
        self.http
            .complete_read(completion)
            .expect("root routed read completion");
        self.http_ready = true;
        self.http_first = true;
    }

    pub fn complete_write(&mut self, completion: http::WriteCompletion<Buffer>) {
        self.http
            .complete_write(completion)
            .expect("root routed write completion");
        self.http_ready = true;
        self.http_first = true;
    }

    pub fn complete_readiness(&mut self, completion: http::ReadinessCompletion) {
        self.http
            .complete_readiness(completion)
            .expect("root routed readiness completion");
        self.http_ready = true;
        self.http_first = true;
    }

    pub fn complete_close(&mut self, completion: http::CloseCompletion) {
        self.http
            .complete_close(completion)
            .expect("root routed close completion");
        self.http_ready = true;
        self.http_first = true;
    }

    pub fn complete_file(&mut self, completion: app::Completion) {
        self.app
            .complete(completion)
            .expect("root routed file completion");
        self.app_ready = true;
    }

    pub fn observe_time(&mut self, now: http::Tick) {
        self.http.observe_time(now).expect("monotonic root clock");
    }

    pub fn expire(&mut self, deadline: http::Deadline, now: http::Tick) {
        match self.http.expire(deadline, now) {
            Ok(()) => {
                self.http_ready = true;
                self.http_first = true;
                self.terminate();
            }
            Err(http::CommandError::StaleDeadline) => {}
            Err(error) => panic!("invalid deadline routing: {error}"),
        }
    }

    pub fn shutdown(&mut self, mode: http::ShutdownMode) {
        if self.lifecycle == Lifecycle::Closed {
            return;
        }
        self.http.shutdown(mode);
        self.http_ready = true;
        self.http_first = true;
        if mode == http::ShutdownMode::Abort {
            self.app.abort();
            self.app_ready = true;
            self.lifecycle = Lifecycle::Terminating;
            self.response = None;
        }
    }

    fn terminate(&mut self) {
        self.lifecycle = Lifecycle::Terminating;
        self.response = None;
        self.app.terminate();
        self.app_ready = true;
    }

    fn drive_http<P: Ports>(&mut self, ports: &mut P) -> Option<P::Output> {
        self.http_ready = false;
        self.http_first = false;
        let mut connector = HttpConnector {
            app: &mut self.app,
            app_ready: &mut self.app_ready,
            lifecycle: &mut self.lifecycle,
            release: &mut self.release,
            credit: &mut self.credit,
            response: &mut self.response,
            incoming: &mut self.incoming,
            ports,
        };
        match self.http.next(&mut connector) {
            Some(Step::External(output)) => {
                self.http_ready = self.lifecycle != Lifecycle::Closed;
                Some(output)
            }
            Some(Step::Wake) => {
                self.http_ready = self.lifecycle != Lifecycle::Closed;
                None
            }
            None => None,
        }
    }

    fn select(&self) -> Option<Transition> {
        if self.returned_body.is_some() {
            return Some(Transition::ReturnRejected);
        }
        if self.release.is_some() {
            return Some(Transition::ReleaseIncoming);
        }
        if self.credit.is_some() {
            return Some(Transition::Credit);
        }
        match self.lifecycle {
            Lifecycle::Closed => self.app_ready.then_some(Transition::App),
            Lifecycle::WaitingForApp => self.app_ready.then_some(Transition::App),
            Lifecycle::Terminating => {
                if self.http_first && self.http_ready {
                    Some(Transition::Http)
                } else if self.app_ready {
                    Some(Transition::App)
                } else {
                    self.http_ready.then_some(Transition::Http)
                }
            }
            Lifecycle::Serving => {
                if self.http_first && self.http_ready {
                    Some(Transition::Http)
                } else if self.response.is_some() && matches!(self.incoming, Incoming::Complete(_))
                {
                    Some(Transition::Respond)
                } else if self.app_ready {
                    Some(Transition::App)
                } else {
                    self.http_ready.then_some(Transition::Http)
                }
            }
        }
    }

    fn commit<P: Ports>(&mut self, transition: Transition, ports: &mut P) -> Option<P::Output> {
        match transition {
            Transition::ReturnRejected => {
                let (exchange, buffer) = self.returned_body.take().unwrap();
                self.app
                    .body_sent(exchange, buffer, 0)
                    .expect("return rejected application lease");
                self.app_ready = true;
            }
            Transition::ReleaseIncoming => {
                let completion = self.release.take().unwrap();
                self.http
                    .release_body(completion)
                    .expect("return incoming lease");
                self.http_ready = true;
            }
            Transition::Credit => {
                let (exchange, count) = self.credit.take().unwrap();
                // A terminal observation can retire the exchange before
                // the connector returns its unused delivery credit.
                if self.lifecycle == Lifecycle::Serving
                    && self.http.grant_body_credit(exchange, count).is_err()
                {
                    let _ = self.http.fail_source(exchange, http::Failure::Application);
                }
                self.http_ready = true;
            }
            Transition::Http => return self.drive_http(ports),
            Transition::Respond => {
                let response = self.response.take().unwrap();
                let Incoming::Complete(exchange) = self.incoming else {
                    unreachable!()
                };
                assert_eq!(exchange, response.exchange);
                match respond(&mut self.http, response) {
                    Ok(()) => self.http_ready = true,
                    Err(_) => {
                        if self
                            .http
                            .fail_source(response.exchange, http::Failure::Application)
                            .is_err()
                        {
                            self.app.exchange_finished(response.exchange);
                            self.app_ready = true;
                        }
                        self.http_ready = true;
                    }
                }
            }
            Transition::App => {
                self.app_ready = false;
                let mut connector = AppConnector {
                    http: &mut self.http,
                    http_ready: &mut self.http_ready,
                    lifecycle: &mut self.lifecycle,
                    response: &mut self.response,
                    returned_body: &mut self.returned_body,
                    ports,
                };
                if let Some(output) = self.app.next(&mut connector) {
                    self.app_ready = true;
                    return Some(output);
                }
            }
        }
        None
    }

    pub fn next<P: Ports>(&mut self, ports: &mut P) -> Option<P::Output> {
        loop {
            for _ in 0..64 {
                self.check();
                let transition = self.select()?;
                if let Some(output) = self.commit(transition, ports) {
                    self.check();
                    return Some(output);
                }
            }
            if let Some(output) = ports.yield_turn() {
                return Some(output);
            }
        }
    }
}

struct HttpConnector<'a, P> {
    app: &'a mut app::App<http::ExchangeId>,
    app_ready: &'a mut bool,
    lifecycle: &'a mut Lifecycle,
    release: &'a mut Option<http::BodyCompletion<Buffer>>,
    credit: &'a mut Option<(http::ExchangeId, usize)>,
    response: &'a mut Option<app::Response<http::ExchangeId>>,
    incoming: &'a mut Incoming,
    ports: &'a mut P,
}

impl<P: Ports> http::Ports<Buffer> for HttpConnector<'_, P> {
    type Output = Step<P::Output>;
    fn read(&mut self, op: http::ReadOp<Buffer>) -> Option<Self::Output> {
        self.ports.read(op).map(Step::External)
    }
    fn write(&mut self, op: http::WriteOp<Buffer>) -> Option<Self::Output> {
        self.ports.write(op).map(Step::External)
    }
    fn readiness(&mut self, op: http::ReadinessOp) -> Option<Self::Output> {
        self.ports.readiness(op).map(Step::External)
    }
    fn cancel(&mut self, op: http::CancelOp) -> Option<Self::Output> {
        self.ports.cancel(op).map(Step::External)
    }
    fn close(&mut self, op: http::CloseOp) -> Option<Self::Output> {
        *self.lifecycle = Lifecycle::Terminating;
        *self.response = None;
        self.app.terminate();
        *self.app_ready = true;
        self.ports.close(op).map(Step::External)
    }
    fn body(&mut self, op: http::BodyOp<Buffer>) -> Option<Self::Output> {
        let count = op.bytes().len();
        assert!(self.credit.replace((op.exchange(), count)).is_none());
        assert!(self.release.replace(op.release(count)).is_none());
        Some(Step::Wake)
    }
    fn trailers(&mut self, _: http::ExchangeId, _: http::Headers<'_>) -> Option<Self::Output> {
        None
    }
    fn incoming_finished(&mut self, exchange: http::ExchangeId) -> Option<Self::Output> {
        assert!(matches!(*self.incoming, Incoming::Receiving(id) if id == exchange));
        *self.incoming = Incoming::Complete(exchange);
        Some(Step::Wake)
    }
    fn send_ready(&mut self, exchange: http::ExchangeId, capacity: usize) -> Option<Self::Output> {
        self.app.demand(exchange, capacity);
        *self.app_ready = true;
        Some(Step::Wake)
    }
    fn source_finished(&mut self, exchange: http::ExchangeId) -> Option<Self::Output> {
        self.app.source_finished(exchange);
        *self.app_ready = true;
        Some(Step::Wake)
    }
    fn body_sent(&mut self, result: http::BodySent<Buffer>) -> Option<Self::Output> {
        self.app
            .body_sent(result.exchange, result.buffer, result.accepted)
            .expect("return outgoing lease");
        *self.app_ready = true;
        Some(Step::Wake)
    }
    fn exchange_finished(&mut self, result: http::ExchangeFinished) -> Option<Self::Output> {
        *self.response = None;
        *self.incoming = Incoming::None;
        self.app.exchange_finished(result.exchange);
        *self.app_ready = true;
        // A failed exchange can still return its outstanding write lease.
        // Only a reusable exchange needs a gate before the next request.
        *self.lifecycle = if result.reusable {
            if self.app.is_idle() {
                Lifecycle::Serving
            } else {
                Lifecycle::WaitingForApp
            }
        } else {
            Lifecycle::Terminating
        };
        Some(
            self.ports
                .exchange_finished()
                .map_or(Step::Wake, Step::External),
        )
    }
    fn deadline_changed(&mut self, deadline: Option<http::Deadline>) -> Option<Self::Output> {
        self.ports.deadline_changed(deadline).map(Step::External)
    }
    fn upgrade_ready(&mut self, _: http::ExchangeId) -> Option<Self::Output> {
        unreachable!("static service never accepts an upgrade")
    }
    fn closed(&mut self, result: http::ConnectionResult) -> Option<Self::Output> {
        *self.lifecycle = Lifecycle::Closed;
        *self.response = None;
        *self.incoming = Incoming::None;
        self.app.abort();
        *self.app_ready = true;
        Some(self.ports.closed(result).map_or(Step::Wake, Step::External))
    }
}

impl<P: Ports> http::ServerPorts<Buffer> for HttpConnector<'_, P> {
    fn request(
        &mut self,
        exchange: http::ExchangeId,
        head: http::RequestHead<'_>,
    ) -> Option<Self::Output> {
        assert_eq!(*self.lifecycle, Lifecycle::Serving);
        *self.incoming = Incoming::Receiving(exchange);
        assert!(
            self.app
                .request(exchange, head.method.as_bytes(), head.target.as_bytes())
        );
        assert!(self.credit.replace((exchange, app::CHUNK_SIZE)).is_none());
        *self.app_ready = true;
        Some(Step::Wake)
    }
}

struct AppConnector<'a, P> {
    http: &'a mut http::Server<Buffer>,
    http_ready: &'a mut bool,
    lifecycle: &'a mut Lifecycle,
    response: &'a mut Option<app::Response<http::ExchangeId>>,
    returned_body: &'a mut Option<(http::ExchangeId, Buffer)>,
    ports: &'a mut P,
}

impl<P: Ports> app::Ports<http::ExchangeId> for AppConnector<'_, P> {
    type Output = P::Output;
    fn open(&mut self, op: app::Open) -> Option<Self::Output> {
        self.ports.open(op)
    }
    fn stat(&mut self, op: app::Stat) -> Option<Self::Output> {
        self.ports.stat(op)
    }
    fn read(&mut self, op: app::Read) -> Option<Self::Output> {
        self.ports.file_read(op)
    }
    fn close(&mut self, op: app::Close) -> Option<Self::Output> {
        self.ports.file_close(op)
    }
    fn respond(&mut self, response: app::Response<http::ExchangeId>) -> Option<Self::Output> {
        // This service drains incoming bodies before its final response.
        // Otherwise the HTTP core correctly treats it as an early response
        // and closes the connection rather than reusing unread input.
        if *self.lifecycle == Lifecycle::Serving {
            assert!(self.response.replace(response).is_none());
        }
        None
    }

    fn body(&mut self, body: app::Body<http::ExchangeId>) -> Option<Self::Output> {
        if let Err(rejected) = self.http.send_body(http::SendBody {
            exchange: body.exchange,
            buffer: body.buffer,
            range: body.range,
            end: body.end,
        }) {
            // Demand can be revoked while the file read is outstanding.
            // Return the unaccepted buffer after App::next releases its borrow.
            let command = rejected.value;
            assert!(
                self.returned_body
                    .replace((command.exchange, command.buffer))
                    .is_none()
            );
        }
        *self.http_ready = true;
        None
    }
    fn source_failed(&mut self, exchange: http::ExchangeId) -> Option<Self::Output> {
        *self.response = None;
        if *self.lifecycle != Lifecycle::Closed {
            let _ = self.http.fail_source(exchange, http::Failure::Application);
            *self.http_ready = true;
            *self.lifecycle = Lifecycle::Terminating;
        }
        None
    }
    fn settled(&mut self, _: http::ExchangeId) -> Option<Self::Output> {
        if *self.lifecycle == Lifecycle::WaitingForApp {
            *self.lifecycle = Lifecycle::Serving;
            *self.http_ready = true;
        }
        None
    }
}

fn respond(
    server: &mut http::Server<Buffer>,
    response: app::Response<http::ExchangeId>,
) -> Result<(), http::CommandError> {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    let headers = [
        http::Header {
            name: "Content-Type",
            value: b"application/octet-stream",
        },
        http::Header {
            name: "Allow",
            value: b"GET, HEAD",
        },
    ];
    server.respond(
        response.exchange,
        http::Response::new(
            response.status,
            reason,
            &headers,
            http::BodyLength::Known(response.length),
        ),
    )
}
