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

pub struct Service {
    http: http::Server<Buffer>,
    app: app::App<http::ExchangeId>,
    http_ready: bool,
    http_first: bool,
    app_ready: bool,
    pause_http: bool,
    release: Option<http::BodyCompletion<Buffer>>,
    credit: Option<(http::ExchangeId, usize)>,
    response: Option<app::Response<http::ExchangeId>>,
    incoming_done: bool,
    returned_body: Option<(http::ExchangeId, Buffer)>,
    closed: bool,
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
            pause_http: false,
            release: None,
            credit: None,
            response: None,
            incoming_done: false,
            returned_body: None,
            closed: false,
        })
    }

    pub fn settled(&self) -> bool {
        self.closed && self.app.is_idle()
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
                self.pause_http = false;
            }
            Err(http::CommandError::StaleDeadline) => {}
            Err(error) => panic!("invalid deadline routing: {error}"),
        }
    }

    pub fn shutdown(&mut self, mode: http::ShutdownMode) {
        self.http.shutdown(mode);
        self.http_ready = true;
        self.http_first = true;
        if mode == http::ShutdownMode::Abort {
            self.app.abort();
            self.app_ready = true;
            self.pause_http = false;
        }
    }

    fn drive_http<P: Ports>(&mut self, ports: &mut P) -> Option<P::Output> {
        self.http_ready = false;
        self.http_first = false;
        let mut connector = HttpConnector {
            app: &mut self.app,
            app_ready: &mut self.app_ready,
            pause_http: &mut self.pause_http,
            release: &mut self.release,
            credit: &mut self.credit,
            closed: &mut self.closed,
            response: &mut self.response,
            incoming_done: &mut self.incoming_done,
            ports,
        };
        match self.http.next(&mut connector) {
            Some(Step::External(output)) => {
                self.http_ready = true;
                Some(output)
            }
            Some(Step::Wake) => {
                self.http_ready = true;
                None
            }
            None => None,
        }
    }

    pub fn next<P: Ports>(&mut self, ports: &mut P) -> Option<P::Output> {
        loop {
            for _ in 0..64 {
                if let Some((exchange, buffer)) = self.returned_body.take() {
                    self.app
                        .body_sent(exchange, buffer, 0)
                        .expect("return rejected application lease");
                    self.app_ready = true;
                }
                if let Some(completion) = self.release.take() {
                    self.http
                        .release_body(completion)
                        .expect("return incoming lease");
                    self.http_ready = true;
                }
                if let Some((exchange, count)) = self.credit.take() {
                    // A terminal observation can retire the exchange before
                    // the connector returns its unused delivery credit.
                    if self.http.grant_body_credit(exchange, count).is_err() {
                        let _ = self.http.fail_source(exchange, http::Failure::Application);
                    }
                    self.http_ready = true;
                }
                if self.http_first && self.http_ready && !self.pause_http {
                    if let Some(output) = self.drive_http(ports) {
                        return Some(output);
                    }
                    continue;
                }
                if self.incoming_done
                    && let Some(response) = self.response.take()
                {
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
                if self.app_ready {
                    self.app_ready = false;
                    let mut connector = AppConnector {
                        http: &mut self.http,
                        http_ready: &mut self.http_ready,
                        pause_http: &mut self.pause_http,
                        response: &mut self.response,
                        returned_body: &mut self.returned_body,
                        ports,
                    };
                    if let Some(output) = self.app.next(&mut connector) {
                        self.app_ready = true;
                        return Some(output);
                    }
                } else if self.http_ready && !self.pause_http {
                    if let Some(output) = self.drive_http(ports) {
                        return Some(output);
                    }
                } else {
                    return None;
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
    pause_http: &'a mut bool,
    release: &'a mut Option<http::BodyCompletion<Buffer>>,
    credit: &'a mut Option<(http::ExchangeId, usize)>,
    closed: &'a mut bool,
    response: &'a mut Option<app::Response<http::ExchangeId>>,
    incoming_done: &'a mut bool,
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
        self.ports.close(op).map(Step::External)
    }
    fn body(&mut self, op: http::BodyOp<Buffer>) -> Option<Self::Output> {
        let count = op.bytes().len();
        *self.credit = Some((op.exchange(), count));
        *self.release = Some(op.release(count));
        Some(Step::Wake)
    }
    fn trailers(&mut self, _: http::ExchangeId, _: http::Headers<'_>) -> Option<Self::Output> {
        None
    }
    fn incoming_finished(&mut self, _: http::ExchangeId) -> Option<Self::Output> {
        *self.incoming_done = true;
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
        self.app.exchange_finished(result.exchange);
        *self.app_ready = true;
        // A failed exchange can still return its outstanding write lease.
        // Only a reusable exchange needs a gate before the next request.
        *self.pause_http = result.reusable && !self.app.is_idle();
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
        *self.closed = true;
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
        *self.incoming_done = false;
        assert!(
            self.app
                .request(exchange, head.method.as_bytes(), head.target.as_bytes())
        );
        *self.credit = Some((exchange, app::CHUNK_SIZE));
        *self.app_ready = true;
        Some(Step::Wake)
    }
}

struct AppConnector<'a, P> {
    http: &'a mut http::Server<Buffer>,
    http_ready: &'a mut bool,
    pause_http: &'a mut bool,
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
        assert!(self.response.replace(response).is_none());
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
        let _ = self.http.fail_source(exchange, http::Failure::Application);
        *self.http_ready = true;
        *self.pause_http = false;
        None
    }
    fn settled(&mut self, _: http::ExchangeId) -> Option<Self::Output> {
        *self.pause_http = false;
        *self.http_ready = true;
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
