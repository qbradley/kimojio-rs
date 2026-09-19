use super::*;

enum Action {
    Read(http::ReadOp<Buffer>),
    Write(http::WriteOp<Buffer>),
    Close(http::CloseOp),
    Open(app::Open),
    Stat(app::Stat),
    FileRead(app::Read),
    FileClose(app::Close),
    Closed,
    Yield,
}

#[derive(Default)]
struct Executor {
    deadline: Option<http::Deadline>,
    closed: Option<http::ConnectionResult>,
    file_closes: usize,
    socket_closes: usize,
    writes: usize,
}

impl Ports for Executor {
    type Output = Action;
    fn read(&mut self, op: http::ReadOp<Buffer>) -> Option<Action> {
        Some(Action::Read(op))
    }
    fn write(&mut self, op: http::WriteOp<Buffer>) -> Option<Action> {
        self.writes += 1;
        Some(Action::Write(op))
    }
    fn readiness(&mut self, _: http::ReadinessOp) -> Option<Action> {
        panic!("no WouldBlock result was supplied")
    }
    fn cancel(&mut self, _: http::CancelOp) -> Option<Action> {
        panic!("all outstanding transport operations already settled")
    }
    fn close(&mut self, op: http::CloseOp) -> Option<Action> {
        self.socket_closes += 1;
        Some(Action::Close(op))
    }
    fn open(&mut self, op: app::Open) -> Option<Action> {
        Some(Action::Open(op))
    }
    fn stat(&mut self, op: app::Stat) -> Option<Action> {
        Some(Action::Stat(op))
    }
    fn file_read(&mut self, op: app::Read) -> Option<Action> {
        Some(Action::FileRead(op))
    }
    fn file_close(&mut self, op: app::Close) -> Option<Action> {
        self.file_closes += 1;
        Some(Action::FileClose(op))
    }
    fn deadline_changed(&mut self, deadline: Option<http::Deadline>) -> Option<Action> {
        self.deadline = deadline;
        None
    }
    fn exchange_finished(&mut self) -> Option<Action> {
        None
    }
    fn closed(&mut self, result: http::ConnectionResult) -> Option<Action> {
        assert!(self.closed.replace(result).is_none());
        Some(Action::Closed)
    }
    fn yield_turn(&mut self) -> Option<Action> {
        Some(Action::Yield)
    }
}

fn service() -> Service {
    Service::new(
        1,
        http::Config {
            head_timeout_ns: Some(100),
            body_timeout_ns: Some(100),
            idle_timeout_ns: Some(100),
            ..http::Config::default()
        },
        http::Tick(0),
    )
    .unwrap()
}

fn feed(service: &mut Service, mut op: http::ReadOp<Buffer>, bytes: &[u8]) {
    op.bytes_mut()[..bytes.len()].copy_from_slice(bytes);
    service.complete_read(op.complete(Ok(bytes.len())));
}

fn file_read_pending() -> (Service, Executor, app::Read) {
    let mut service = service();
    let mut executor = Executor::default();
    for _ in 0..100 {
        match service.next(&mut executor).expect("setup is runnable") {
            Action::Read(op) => feed(
                &mut service,
                op,
                b"GET /file HTTP/1.1\r\nHost: localhost\r\n\r\n",
            ),
            Action::Open(op) => service.complete_file(app::Completion::Open {
                id: op.id,
                result: Ok(app::File(3)),
            }),
            Action::Stat(op) => service.complete_file(app::Completion::Stat {
                id: op.id,
                result: Ok(app::Metadata {
                    length: 4,
                    regular: true,
                }),
            }),
            Action::Write(op) => {
                let count = op.slices().iter().map(|slice| slice.len()).sum();
                service.complete_write(op.complete(Ok(count)));
            }
            Action::FileRead(op) => {
                assert_eq!(executor.writes, 1);
                return (service, executor, op);
            }
            Action::Yield => {}
            _ => panic!("unexpected setup action"),
        }
    }
    panic!("file read did not become pending");
}

fn complete_file_read(service: &mut Service, mut op: app::Read) {
    op.buffer[..4].copy_from_slice(b"data");
    service.complete_file(app::Completion::Read {
        id: op.id,
        buffer: op.buffer,
        result: Ok(4),
    });
}

fn settle(service: &mut Service, executor: &mut Executor) {
    let initial_writes = executor.writes;
    for _ in 0..100 {
        if service.settled() {
            assert_eq!(
                executor.writes, initial_writes,
                "revoked body reached transport"
            );
            assert_eq!(executor.file_closes, 1);
            assert_eq!(executor.socket_closes, 1);
            assert!(service.returned_body.is_none());
            assert!(
                service.app.is_idle(),
                "application still owns an outstanding lease"
            );
            return;
        }
        match service
            .next(executor)
            .expect("settlement has no external pending work")
        {
            Action::FileClose(op) => service.complete_file(app::Completion::Close {
                id: op.id,
                result: Ok(()),
            }),
            Action::Close(op) => service.complete_close(op.complete(Ok(()))),
            Action::Closed | Action::Yield => {}
            _ => panic!("unexpected operation after admission revocation"),
        }
    }
    panic!("resource settlement stalled");
}

#[test]
fn expiry_before_successful_file_completion_settles_without_sending_body() {
    let (mut service, mut executor, read) = file_read_pending();
    let deadline = executor.deadline.expect("body deadline");
    service.expire(deadline, deadline.at);
    complete_file_read(&mut service, read);
    settle(&mut service, &mut executor);
    assert_eq!(executor.closed, Some(Err(http::Failure::Timeout)));
}

#[test]
fn cancellation_before_successful_file_completion_settles_once() {
    let (mut service, mut executor, read) = file_read_pending();
    service.shutdown(http::ShutdownMode::Abort);
    complete_file_read(&mut service, read);
    settle(&mut service, &mut executor);
    assert_eq!(executor.closed, Some(Err(http::Failure::Cancelled)));
}

#[test]
fn rejected_body_returns_lease_even_without_terminal_notification_priority() {
    let (mut service, mut executor, read) = file_read_pending();
    // Revoke core admission without publishing a ready notification. This
    // forces the connector's rejection path instead of relying on scheduling.
    service.http.shutdown(http::ShutdownMode::Abort);
    service.http_ready = false;
    service.http_first = false;
    complete_file_read(&mut service, read);
    settle(&mut service, &mut executor);
    assert_eq!(executor.closed, Some(Err(http::Failure::Cancelled)));
}

#[test]
fn disconnect_before_late_file_metadata_does_not_submit_a_response() {
    let mut service = service();
    let mut executor = Executor::default();
    let mut metadata = None;
    let mut header_sent = false;
    let read = loop {
        match service.next(&mut executor).expect("setup is runnable") {
            Action::Read(op) if !header_sent => {
                header_sent = true;
                feed(
                    &mut service,
                    op,
                    b"GET /file HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\n",
                );
            }
            Action::Read(op) => break op,
            Action::Open(op) => service.complete_file(app::Completion::Open {
                id: op.id,
                result: Ok(app::File(3)),
            }),
            Action::Stat(op) => metadata = Some(op),
            Action::Yield => {}
            _ => panic!("unexpected setup action"),
        }
    };
    let error = http::IoError {
        kind: http::IoErrorKind::Reset,
        code: Some(104),
    };
    service.complete_read(read.complete(Err(error)));
    let op = metadata.expect("metadata operation was pending");
    service.complete_file(app::Completion::Stat {
        id: op.id,
        result: Ok(app::Metadata {
            length: 4,
            regular: true,
        }),
    });
    settle(&mut service, &mut executor);
    assert_eq!(executor.writes, 0);
    assert_eq!(executor.closed, Some(Err(http::Failure::Transport(error))));
}

#[test]
fn final_response_is_admitted_once_while_continue_write_is_outstanding() {
    let mut service = service();
    let mut executor = Executor::default();
    let mut header_sent = false;
    let mut metadata_done = false;
    let mut body_read = None;
    let mut continue_write = None;
    for _ in 0..100 {
        if metadata_done && body_read.is_some() && continue_write.is_some() {
            break;
        }
        match service.next(&mut executor).expect("setup is runnable") {
            Action::Read(op) if !header_sent => {
                header_sent = true;
                feed(&mut service, op, b"GET /file HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n");
            }
            Action::Read(op) => body_read = Some(op),
            Action::Open(op) => service.complete_file(app::Completion::Open {
                id: op.id,
                result: Ok(app::File(3)),
            }),
            Action::Stat(op) => {
                metadata_done = true;
                service.complete_file(app::Completion::Stat {
                    id: op.id,
                    result: Ok(app::Metadata {
                        length: 4,
                        regular: true,
                    }),
                });
            }
            Action::Write(op) => {
                assert!(op.slices()[0].starts_with(b"HTTP/1.1 100 "));
                assert!(continue_write.replace(op).is_none());
            }
            Action::Yield => {}
            _ => panic!("unexpected setup action"),
        }
    }
    assert!(metadata_done);
    feed(&mut service, body_read.expect("request body read"), b"body");
    assert!(service.next(&mut executor).is_none());
    assert!(
        service.response.is_none(),
        "adapter retained a retry instead of admitting the final head"
    );
    assert_eq!(executor.writes, 1, "the 100 write still owns transport");
    let op = continue_write.expect("100 response write");
    let mut wire = op.slices().concat();
    service.complete_write(op.complete(Ok(wire.len())));

    for _ in 0..100 {
        if service.settled() {
            break;
        }
        match service.next(&mut executor).expect("completion is runnable") {
            Action::Write(op) => {
                let bytes = op.slices().concat();
                let count = bytes.len();
                wire.extend_from_slice(&bytes);
                service.complete_write(op.complete(Ok(count)));
            }
            Action::FileRead(op) => complete_file_read(&mut service, op),
            Action::FileClose(op) => service.complete_file(app::Completion::Close {
                id: op.id,
                result: Ok(()),
            }),
            Action::Close(op) => service.complete_close(op.complete(Ok(()))),
            Action::Closed | Action::Yield => {}
            _ => panic!("unexpected completion action"),
        }
    }
    assert!(service.settled());
    assert_eq!(executor.closed, Some(Ok(())));
    assert_eq!(executor.file_closes, 1);
    assert_eq!(executor.socket_closes, 1);
    assert_eq!(
        wire.windows(b"HTTP/1.1 100".len())
            .filter(|bytes| *bytes == b"HTTP/1.1 100")
            .count(),
        1
    );
    assert_eq!(
        wire.windows(b"HTTP/1.1 200".len())
            .filter(|bytes| *bytes == b"HTTP/1.1 200")
            .count(),
        1
    );
    assert!(wire.ends_with(b"data"));
}
