//! A resource-ledger oracle, not a second copy of the production selectors.
//! External completions run only between bounded drives. Callback yields do
//! not change that schedule. Real cancellation CQEs are covered by ring.rs.
use http1_static::{app, composite};
use kimojio_fsm_http1 as http;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Open,
    Stat,
    Read,
    Write,
}

#[derive(Debug, Eq, PartialEq)]
enum Notice {
    Read(http::OperationId),
    Write(http::OperationId, Vec<u8>),
    Cancel(http::OperationId),
    SocketClose(http::OperationId),
    Open(app::Id, Vec<u8>),
    Stat(app::Id, app::File),
    FileRead(app::Id, app::File, u64, usize),
    FileClose(app::Id, app::File),
    Deadline(Option<http::Deadline>),
    Exchange,
    Closed(http::ConnectionResult),
}

struct Ledger {
    mask: u64,
    calls: usize,
    turns: usize,
    trace: Vec<Notice>,
    read: Option<http::ReadOp<app::Buffer>>,
    write: Option<http::WriteOp<app::Buffer>>,
    socket_close: Option<http::CloseOp>,
    open: Option<app::Open>,
    stat: Option<app::Stat>,
    file_read: Option<app::Read>,
    file_close: Option<app::Close>,
    file_owned: bool,
    socket_owned: bool,
    file_pointer: Option<*const u8>,
    deadline: Option<http::Deadline>,
    cancel_acks: usize,
    closed: usize,
    exchanges: usize,
    file_closes: usize,
    socket_closes: usize,
    terminating: bool,
}

impl Ledger {
    fn new(mask: u64) -> Self {
        Self {
            mask,
            calls: 0,
            turns: 0,
            trace: Vec::new(),
            read: None,
            write: None,
            socket_close: None,
            open: None,
            stat: None,
            file_read: None,
            file_close: None,
            file_owned: false,
            socket_owned: true,
            file_pointer: None,
            deadline: None,
            cancel_acks: 0,
            closed: 0,
            exchanges: 0,
            file_closes: 0,
            socket_closes: 0,
            terminating: false,
        }
    }

    fn record(&mut self, notice: Notice) -> Option<()> {
        self.trace.push(notice);
        let yield_now = self.mask & (1 << (self.calls % 64)) != 0;
        self.calls += 1;
        yield_now.then_some(())
    }

    fn file_original(&self) -> bool {
        self.open.is_some() || self.stat.is_some() || self.file_read.is_some()
    }

    fn original(&self, stage: Stage) -> bool {
        match stage {
            Stage::Open => self.open.is_some(),
            Stage::Stat => self.stat.is_some(),
            Stage::Read => self.file_read.is_some(),
            Stage::Write => self.write.is_some(),
        }
    }

    fn complete_original(&mut self, service: &mut composite::Service, stage: Stage, result: usize) {
        let completion = match stage {
            Stage::Open => {
                let op = self.open.take().unwrap();
                self.file_owned = result != 0;
                app::Completion::Open {
                    id: op.id,
                    result: if result == 0 {
                        Err(app::FileError::Other)
                    } else {
                        Ok(app::File(3))
                    },
                }
            }
            Stage::Stat => {
                let op = self.stat.take().unwrap();
                app::Completion::Stat {
                    id: op.id,
                    result: if result == 0 {
                        Err(app::FileError::Other)
                    } else {
                        Ok(app::Metadata {
                            length: 4,
                            regular: true,
                        })
                    },
                }
            }
            Stage::Read => {
                let mut op = self.file_read.take().unwrap();
                assert_eq!(Some(op.buffer.as_ptr()), self.file_pointer);
                op.buffer[..4].copy_from_slice(b"data");
                app::Completion::Read {
                    id: op.id,
                    buffer: op.buffer,
                    result: if result == 0 {
                        Err(app::FileError::Other)
                    } else {
                        Ok(result)
                    },
                }
            }
            Stage::Write => {
                let op = self.write.take().unwrap();
                assert_eq!(op.slices().concat(), b"data");
                service.complete_write(op.complete(if result == 0 {
                    Err(http::IoError {
                        kind: http::IoErrorKind::Cancelled,
                        code: None,
                    })
                } else {
                    Ok(result)
                }));
                return;
            }
        };
        service.complete_file(completion);
    }
}

impl composite::Ports for Ledger {
    type Output = ();

    fn read(&mut self, op: http::ReadOp<app::Buffer>) -> Option<()> {
        assert!(self.socket_owned && !self.terminating);
        let id = op.id();
        assert!(self.read.replace(op).is_none());
        self.record(Notice::Read(id))
    }
    fn write(&mut self, op: http::WriteOp<app::Buffer>) -> Option<()> {
        assert!(self.socket_owned);
        let bytes = op.slices().concat();
        if bytes == b"data" {
            assert!(
                op.slices()
                    .iter()
                    .any(|bytes| Some(bytes.as_ptr()) == self.file_pointer)
            );
        }
        if self.terminating {
            assert!(
                bytes.starts_with(b"HTTP/1.1 408 "),
                "new producer output after termination"
            );
        }
        let id = op.id();
        assert!(self.write.replace(op).is_none());
        self.record(Notice::Write(id, bytes))
    }
    fn readiness(&mut self, _: http::ReadinessOp) -> Option<()> {
        panic!("no WouldBlock completions in this model")
    }
    fn cancel(&mut self, op: http::CancelOp) -> Option<()> {
        assert!(
            self.read
                .as_ref()
                .is_some_and(|read| read.id() == op.target)
                || self
                    .write
                    .as_ref()
                    .is_some_and(|write| write.id() == op.target)
        );
        self.cancel_acks += 1;
        self.record(Notice::Cancel(op.target))
    }
    fn close(&mut self, op: http::CloseOp) -> Option<()> {
        assert!(
            self.read.is_none() && self.write.is_none(),
            "close before original completion"
        );
        assert!(self.socket_owned);
        self.socket_owned = false;
        self.socket_closes += 1;
        let id = op.id();
        assert!(self.socket_close.replace(op).is_none());
        self.record(Notice::SocketClose(id))
    }
    fn open(&mut self, op: app::Open) -> Option<()> {
        assert!(!self.terminating && !self.file_owned && !self.file_original());
        let notice = Notice::Open(op.id, op.path.clone());
        self.open = Some(op);
        self.record(notice)
    }
    fn stat(&mut self, op: app::Stat) -> Option<()> {
        assert!(!self.terminating && self.file_owned && !self.file_original());
        let notice = Notice::Stat(op.id, op.file);
        self.stat = Some(op);
        self.record(notice)
    }
    fn file_read(&mut self, op: app::Read) -> Option<()> {
        assert!(!self.terminating && self.file_owned && !self.file_original());
        assert_eq!((op.offset, op.limit), (0, 4));
        self.file_pointer = Some(op.buffer.as_ptr());
        let notice = Notice::FileRead(op.id, op.file, op.offset, op.limit);
        self.file_read = Some(op);
        self.record(notice)
    }
    fn file_close(&mut self, op: app::Close) -> Option<()> {
        assert!(
            self.file_owned && !self.file_original(),
            "file close before original completion"
        );
        assert_eq!(op.file, app::File(3));
        self.file_owned = false;
        self.file_closes += 1;
        let notice = Notice::FileClose(op.id, op.file);
        assert!(self.file_close.replace(op).is_none());
        self.record(notice)
    }
    fn deadline_changed(&mut self, deadline: Option<http::Deadline>) -> Option<()> {
        self.deadline = deadline;
        self.record(Notice::Deadline(deadline))
    }
    fn exchange_finished(&mut self) -> Option<()> {
        self.exchanges += 1;
        assert_eq!(self.exchanges, 1);
        self.record(Notice::Exchange)
    }
    fn closed(&mut self, result: http::ConnectionResult) -> Option<()> {
        self.closed += 1;
        assert_eq!(self.closed, 1);
        assert!(!self.socket_owned && self.socket_close.is_none());
        // The native root requests file cancellation here. An acknowledgement
        // affects cancellation capacity, never the original file ownership.
        if self.file_original() {
            self.cancel_acks += 1;
        }
        self.record(Notice::Closed(result))
    }
    fn yield_turn(&mut self) -> Option<()> {
        self.turns += 1;
        Some(())
    }
}

fn drive(service: &mut composite::Service, ledger: &mut Ledger) {
    for _ in 0..100 {
        if service.next(ledger).is_none() {
            return;
        }
    }
    panic!("drive did not reach quiescence");
}

fn setup(stage: Stage, mask: u64) -> (composite::Service, Ledger) {
    let mut service = composite::Service::new(
        1,
        http::Config {
            head_timeout_ns: Some(100),
            body_timeout_ns: Some(100),
            idle_timeout_ns: Some(100),
            ..http::Config::default()
        },
        http::Tick(0),
    )
    .unwrap();
    let mut ledger = Ledger::new(mask);
    drive(&mut service, &mut ledger);
    let mut read = ledger.read.take().unwrap();
    let request = b"GET /file HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    read.bytes_mut()[..request.len()].copy_from_slice(request);
    service.complete_read(read.complete(Ok(request.len())));
    drive(&mut service, &mut ledger);
    if stage != Stage::Open {
        ledger.complete_original(&mut service, Stage::Open, 4);
        drive(&mut service, &mut ledger);
    }
    if matches!(stage, Stage::Read | Stage::Write) {
        ledger.complete_original(&mut service, Stage::Stat, 4);
        drive(&mut service, &mut ledger);
        let write = ledger.write.take().unwrap();
        let count = write.slices().iter().map(|bytes| bytes.len()).sum();
        service.complete_write(write.complete(Ok(count)));
        drive(&mut service, &mut ledger);
    }
    if stage == Stage::Write {
        ledger.complete_original(&mut service, Stage::Read, 4);
        drive(&mut service, &mut ledger);
    }
    assert!(ledger.original(stage));
    (service, ledger)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Completion {
    Original,
    FileClose,
    SocketClose,
    CancelAck,
}

fn run(
    stage: Stage,
    timeout: bool,
    completion_first: bool,
    result: usize,
    order: [Completion; 4],
    mask: u64,
) -> Vec<Notice> {
    let (mut service, mut ledger) = setup(stage, mask);
    let deadline = ledger.deadline.expect("active exchange deadline");
    if completion_first {
        ledger.complete_original(&mut service, stage, result);
    }
    if timeout {
        service.expire(deadline, deadline.at);
    } else {
        service.shutdown(http::ShutdownMode::Abort);
    }
    ledger.terminating = true;
    drive(&mut service, &mut ledger);
    for _ in 0..16 {
        if service.settled() && ledger.cancel_acks == 0 {
            assert!(!ledger.file_owned && !ledger.socket_owned);
            assert!(!ledger.file_original() && ledger.file_close.is_none());
            assert!(
                ledger.read.is_none() && ledger.write.is_none() && ledger.socket_close.is_none()
            );
            assert_eq!(
                ledger.file_closes,
                usize::from(stage != Stage::Open || result != 0)
            );
            assert_eq!(
                (ledger.socket_closes, ledger.closed, ledger.exchanges),
                (1, 1, 1)
            );
            let notices = ledger.trace.len();
            service.shutdown(http::ShutdownMode::Abort);
            drive(&mut service, &mut ledger);
            assert!(service.settled());
            assert_eq!(
                ledger.trace.len(),
                notices,
                "terminal notifications repeated"
            );
            return ledger.trace;
        }
        assert!(!service.settled() || !ledger.file_original());
        let eligible = order
            .into_iter()
            .find(|event| match event {
                Completion::Original => ledger.original(stage) || ledger.write.is_some(),
                Completion::FileClose => ledger.file_close.is_some(),
                Completion::SocketClose => ledger.socket_close.is_some(),
                Completion::CancelAck => ledger.cancel_acks != 0,
            })
            .unwrap_or_else(|| panic!("no completion: {stage:?} timeout={timeout} first={completion_first} result={result} order={order:?} trace={:?}", ledger.trace));
        match eligible {
            Completion::Original if ledger.original(stage) => {
                ledger.complete_original(&mut service, stage, result)
            }
            Completion::Original => {
                let op = ledger.write.take().unwrap();
                let bytes = op.slices().concat();
                assert!(bytes.starts_with(b"HTTP/1.1 408 "));
                service.complete_write(op.complete(Ok(bytes.len())));
            }
            Completion::FileClose => {
                let op = ledger.file_close.take().unwrap();
                service.complete_file(app::Completion::Close {
                    id: op.id,
                    result: Err(app::FileError::Other),
                });
            }
            Completion::SocketClose => {
                let op = ledger.socket_close.take().unwrap();
                service.complete_close(op.complete(Ok(())));
            }
            Completion::CancelAck => {
                let file_pending = ledger.file_original();
                let write_pending = ledger.write.is_some();
                ledger.cancel_acks -= 1;
                drive(&mut service, &mut ledger);
                assert_eq!(ledger.file_original(), file_pending);
                assert_eq!(ledger.write.is_some(), write_pending);
            }
        }
        drive(&mut service, &mut ledger);
    }
    panic!("settlement exceeded the external completion bound");
}

#[test]
fn bounded_completion_and_callback_schedules_obey_resource_joins() {
    use Completion::*;
    let events = [Original, FileClose, SocketClose, CancelAck];
    let mut schedules = 0;
    for a in events {
        for b in events {
            for c in events {
                for d in events {
                    let order = [a, b, c, d];
                    if order
                        .iter()
                        .any(|event| order.iter().filter(|other| *other == event).count() != 1)
                    {
                        continue;
                    }

                    for stage in [Stage::Open, Stage::Stat, Stage::Read, Stage::Write] {
                        for timeout in [false, true] {
                            for completion_first in [false, true] {
                                for result in [0, 2, 4] {
                                    let expected =
                                        run(stage, timeout, completion_first, result, order, 0);
                                    for mask in [u64::MAX, 0xaaaa_aaaa_aaaa_aaaa]
                                        .into_iter()
                                        .chain((0..24).map(|bit| 1 << bit))
                                    {
                                        let actual = run(
                                            stage,
                                            timeout,
                                            completion_first,
                                            result,
                                            order,
                                            mask,
                                        );
                                        assert_eq!(
                                            actual, expected,
                                            "{stage:?} {order:?} timeout={timeout} first={completion_first} result={result} mask={mask:x}"
                                        );
                                        schedules += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(schedules, 29_952);
}

#[test]
fn buffered_chunk_work_yields_without_losing_the_ready_obligation() {
    let mut service = composite::Service::new(1, http::Config::default(), http::Tick(0)).unwrap();
    let mut ledger = Ledger::new(0);
    drive(&mut service, &mut ledger);
    let mut read = ledger.read.take().unwrap();
    let mut request =
        b"GET /file HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    for _ in 0..256 {
        request.extend_from_slice(b"1\r\nx\r\n");
    }
    request.extend_from_slice(b"0\r\n\r\n");
    read.bytes_mut()[..request.len()].copy_from_slice(&request);
    service.complete_read(read.complete(Ok(request.len())));
    drive(&mut service, &mut ledger);
    assert!(ledger.turns > 0, "buffered work monopolized the caller");
    assert!(ledger.open.is_some() && ledger.read.is_none() && ledger.write.is_none());
    service.shutdown(http::ShutdownMode::Abort);
    ledger.terminating = true;
    ledger.complete_original(&mut service, Stage::Open, 0);
    drive(&mut service, &mut ledger);
    let close = ledger.socket_close.take().unwrap();
    service.complete_close(close.complete(Ok(())));
    drive(&mut service, &mut ledger);
    assert!(service.settled());
    assert_eq!((ledger.closed, ledger.exchanges), (1, 1));
}
