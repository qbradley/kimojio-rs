use super::schema::Fields;
use kimojio_fsm_http2::*;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpStream},
    time::{Duration, Instant},
};

#[derive(Debug)]
pub enum Buffer {
    Bytes(Vec<u8>),
    Lease {
        body: BodyOp,
        start: usize,
        end: usize,
    },
}

impl AsRef<[u8]> for Buffer {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Bytes(bytes) => bytes,
            Self::Lease { body, start, end } => &body.bytes()[*start..*end],
        }
    }
}
impl SendBuffer for Buffer {
    fn retained_capacity(&self) -> usize {
        match self {
            Self::Bytes(bytes) => bytes.capacity(),
            Self::Lease { body, .. } => body.retained_capacity(),
        }
    }
}

#[track_caller]
pub fn checked<T, E: std::fmt::Debug>(result: Result<T, E>) -> Result<T, String> {
    let caller = std::panic::Location::caller();
    result.map_err(|e| format!("{e:?} at {caller}"))
}

fn trace(value: impl std::fmt::Debug) {
    if std::env::var_os("H2_FIXTURE_TRACE").is_some() {
        eprintln!("{value:?}");
    }
}

pub enum Event {
    Headers(StreamId, HeadKind, Fields),
    Body(BodyOp),
    Permit(SendPermit),
    Sent(Sent<Buffer>),
    End(ReceiveEnd),
    Retired(StreamResult),
    Stopped(StreamId),
    Cancel(CancelOp),
    Close(CloseOp),
    Closed(ConnectionResult),
    Again,
}

/// The public ports have no SETTINGS-ready event. Observe only the first frame
/// boundary; the engine still validates and interprets every received byte.
#[derive(Default)]
struct SettingsBoundary {
    header: [u8; 9],
    header_bytes: usize,
    remaining: Option<usize>,
    received: bool,
}

impl SettingsBoundary {
    fn observe(&mut self, mut bytes: &[u8]) {
        if self.received {
            return;
        }
        let copied = (9 - self.header_bytes).min(bytes.len());
        self.header[self.header_bytes..self.header_bytes + copied]
            .copy_from_slice(&bytes[..copied]);
        self.header_bytes += copied;
        bytes = &bytes[copied..];
        if self.header_bytes != 9 {
            return;
        }
        if self.remaining.is_none() {
            let length = usize::from(self.header[0]) << 16
                | usize::from(self.header[1]) << 8
                | usize::from(self.header[2]);
            if self.header[3] != 4
                || self.header[4] & 1 != 0
                || self.header[5] & 0x7f != 0
                || self.header[6..] != [0, 0, 0]
                || length > 16384
                || length % 6 != 0
            {
                return;
            }
            self.remaining = Some(length);
        }
        let remaining = self.remaining.as_mut().unwrap();
        *remaining = remaining.saturating_sub(bytes.len());
        self.received = *remaining == 0;
    }
}

pub struct Transport {
    socket: Option<TcpStream>,
    read: Option<ReadOp>,
    write: Option<WriteOp<Buffer>>,
    alarms: Vec<WakeOp>,
    origin: Instant,
    settings_boundary: SettingsBoundary,
    pub physically_closed: bool,
}

impl Transport {
    pub fn new(socket: TcpStream) -> Result<Self, String> {
        checked(socket.set_nonblocking(true))?;
        checked(socket.set_nodelay(true))?;
        Ok(Self {
            socket: Some(socket),
            read: None,
            write: None,
            alarms: Vec::new(),
            origin: Instant::now(),
            settings_boundary: SettingsBoundary::default(),
            physically_closed: false,
        })
    }
    pub fn now(&self) -> Duration {
        self.origin.elapsed()
    }
    pub fn initial_settings_received(&self) -> bool {
        self.settings_boundary.received
    }

    /// Each turn tries both directions once, even while the engine has ready work.
    pub fn io(&mut self, core: &mut Connection<Buffer>) -> Result<bool, String> {
        checked(core.advance_time(self.now()))?;
        let mut progress = false;
        if let Some(socket) = self.socket.as_mut() {
            if let Some(mut op) = self.read.take() {
                let outcome = match socket.read(op.buffer_mut()) {
                    Ok(0) => Some(ReadOutcome::Eof),
                    Ok(n) => Some(ReadOutcome::Read(n)),
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) =>
                    {
                        None
                    }
                    Err(_) => Some(ReadOutcome::Failed(IoFailure::Failed)),
                };
                if let Some(outcome) = outcome {
                    if let ReadOutcome::Read(n) = outcome {
                        self.settings_boundary.observe(&op.buffer_mut()[..n]);
                    }
                    trace(("read complete", op.token().sequence(), outcome));
                    checked(core.complete_read(op.complete(outcome)))?;
                    progress = true;
                } else {
                    self.read = Some(op);
                }
            }
            if let Some(op) = self.write.take() {
                let outcome = match socket.write_vectored(&op.slices()) {
                    Ok(0) => Some(WriteOutcome::Failed {
                        progress: Progress::Exact(0),
                        error: IoFailure::Failed,
                    }),
                    Ok(n) => Some(WriteOutcome::Written(n)),
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) =>
                    {
                        None
                    }
                    Err(_) => Some(WriteOutcome::Failed {
                        progress: Progress::Exact(0),
                        error: IoFailure::Failed,
                    }),
                };
                if let Some(outcome) = outcome {
                    trace(("write complete", op.token().sequence(), outcome));
                    checked(core.complete_write(op.complete(outcome)))?;
                    progress = true;
                } else {
                    self.write = Some(op);
                }
            }
        }
        let now = self.now();
        let mut index = 0;
        while index < self.alarms.len() {
            if self.alarms[index].deadline() <= now {
                let alarm = self.alarms.swap_remove(index);
                checked(core.complete_wake(alarm.complete(now)))?;
                progress = true;
            } else {
                index += 1;
            }
        }
        Ok(progress)
    }

    pub fn cancel(
        &mut self,
        core: &mut Connection<Buffer>,
        cancel: CancelOp,
    ) -> Result<(), String> {
        // No syscall is in flight: these original operations can settle exactly.
        if self
            .read
            .as_ref()
            .is_some_and(|op| op.token() == cancel.original())
        {
            let op = self.read.take().unwrap();
            checked(core.complete_read(op.complete(ReadOutcome::Failed(IoFailure::Cancelled))))?;
        }
        if self
            .write
            .as_ref()
            .is_some_and(|op| op.token() == cancel.original())
        {
            let op = self.write.take().unwrap();
            checked(core.complete_write(op.complete(WriteOutcome::Failed {
                progress: Progress::Exact(0),
                error: IoFailure::Cancelled,
            })))?;
        }
        if let Some(index) = self
            .alarms
            .iter()
            .position(|op| op.token() == cancel.original())
        {
            let alarm = self.alarms.swap_remove(index);
            checked(core.complete_wake(alarm.complete(self.now())))?;
        }
        checked(core.complete_cancel(cancel.complete()))
    }

    pub fn close(&mut self, core: &mut Connection<Buffer>, op: CloseOp) -> Result<(), String> {
        if self.read.is_some() || self.write.is_some() {
            return Err("CloseOp issued before original I/O settlement".into());
        }
        let socket = self.socket.take().ok_or("duplicate CloseOp")?;
        // Shutdown is best effort after peer reset; dropping closes the actual fd.
        let _ = socket.shutdown(Shutdown::Both);
        drop(socket);
        self.physically_closed = true;
        checked(core.complete_close(op.complete(Ok(()))))
    }

    pub fn wait(&self, watchdog: Duration) -> Result<(), String> {
        let now = self.now();
        let deadline = self
            .alarms
            .iter()
            .map(WakeOp::deadline)
            .min()
            .unwrap_or(watchdog)
            .min(watchdog);
        let delay = deadline.saturating_sub(now).min(Duration::from_millis(50));
        let timeout = Timespec {
            tv_sec: delay.as_secs() as _,
            tv_nsec: delay.subsec_nanos() as _,
        };
        if let Some(socket) = &self.socket {
            let mut flags = PollFlags::empty();
            if self.read.is_some() {
                flags |= PollFlags::IN;
            }
            if self.write.is_some() {
                flags |= PollFlags::OUT;
            }
            let mut fds = [PollFd::new(socket, flags)];
            match poll(&mut fds, Some(&timeout)) {
                Ok(_) | Err(rustix::io::Errno::INTR) => Ok(()),
                Err(error) => Err(error.to_string()),
            }
        } else {
            std::thread::sleep(delay);
            Ok(())
        }
    }

    pub fn drain(&mut self, core: &mut Connection<Buffer>) -> Result<(), String> {
        let _ = core.shutdown();
        let deadline = self.now() + Duration::from_secs(2);
        while self.now() < deadline {
            let progress = self.io(core)?;
            match core.next(self) {
                Some(Event::Body(body)) => checked(core.release_body(body.release()))?,
                Some(Event::Sent(Sent {
                    buffer: Buffer::Lease { body, .. },
                    ..
                })) => {
                    checked(core.release_body(body.release()))?;
                }
                Some(Event::Cancel(cancel)) => self.cancel(core, cancel)?,
                Some(Event::Close(close)) => self.close(core, close)?,
                Some(Event::Closed(_)) => return Ok(()),
                Some(_) => (),
                None if !progress => self.wait(deadline)?,
                None => (),
            }
        }
        Err("cleanup watchdog expired before CloseOp settlement".into())
    }
}

impl Ports<Buffer> for Transport {
    type Output = Event;
    fn read(&mut self, op: ReadOp) -> Option<Event> {
        trace(("read", op.token().sequence()));
        assert!(self.read.replace(op).is_none(), "overlapping reads");
        None
    }

    fn write(&mut self, op: WriteOp<Buffer>) -> Option<Event> {
        trace(("write", op.token().sequence(), op.remaining()));
        assert!(self.write.replace(op).is_none(), "overlapping writes");
        None
    }
    fn headers(&mut self, head: Head<'_>) -> Option<Event> {
        Some(Event::Headers(
            head.stream,
            head.kind,
            head.fields()
                .map(|f| {
                    (
                        String::from_utf8_lossy(f.name).into_owned(),
                        String::from_utf8_lossy(f.value).into_owned(),
                    )
                })
                .collect(),
        ))
    }
    fn body(&mut self, op: BodyOp) -> Option<Event> {
        Some(Event::Body(op))
    }
    fn send_ready(&mut self, op: SendPermit) -> Option<Event> {
        trace(("permit", op.stream()));
        Some(Event::Permit(op))
    }
    fn sent(&mut self, sent: Sent<Buffer>) -> Option<Event> {
        trace((self.now(), "sent", sent.stream, sent.accepted, sent.result));
        Some(Event::Sent(sent))
    }
    fn ended(&mut self, end: ReceiveEnd) -> Option<Event> {
        trace((self.now(), end));
        Some(Event::End(end))
    }
    fn retired(&mut self, result: StreamResult) -> Option<Event> {
        trace((self.now(), result));
        Some(Event::Retired(result))
    }
    fn send_stopped(&mut self, id: StreamId, reason: SendStop) -> Option<Event> {
        trace((self.now(), "stopped", id, reason));
        Some(Event::Stopped(id))
    }
    fn cancel(&mut self, op: CancelOp) -> Option<Event> {
        trace(("cancel", op.original().sequence()));
        Some(Event::Cancel(op))
    }
    fn wake(&mut self, op: WakeOp) -> Option<Event> {
        trace(("wake", op.token().sequence(), op.deadline()));
        self.alarms.push(op);
        None
    }
    fn close(&mut self, op: CloseOp) -> Option<Event> {
        trace(("close", op.token().sequence()));
        Some(Event::Close(op))
    }
    fn closed(&mut self, result: ConnectionResult) -> Option<Event> {
        trace(result);
        Some(Event::Closed(result))
    }
    fn reschedule(&mut self) -> Option<Event> {
        Some(Event::Again)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn settings_boundary_accepts_fragmentation_without_waiting_for_ack() {
        let frame = [0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 4, 0, 0, 4, 0];
        for split in 0..frame.len() {
            let mut boundary = SettingsBoundary::default();
            boundary.observe(&frame[..split]);
            assert!(!boundary.received);
            boundary.observe(&frame[split..]);
            assert!(boundary.received);
        }
        let mut ack = SettingsBoundary::default();
        ack.observe(&[0, 0, 0, 4, 1, 0, 0, 0, 0]);
        assert!(!ack.received);
        let mut reserved = SettingsBoundary::default();
        reserved.observe(&[0, 0, 0, 4, 0, 0x80, 0, 0, 0]);
        assert!(reserved.received);
    }

    #[test]
    fn shutdown_settles_original_operations_and_closes_the_fd() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut transport = Transport::new(socket).unwrap();
        let config = Config {
            shutdown_timeout: Duration::from_millis(10),
            ..Config::default()
        };
        let mut core = Client::<Buffer>::new(config, Duration::ZERO).unwrap();
        assert!(!transport.physically_closed);
        core.next(&mut transport);
        assert!(transport.read.is_some());
        assert!(transport.write.is_some());
        transport.drain(&mut core).unwrap();
        assert!(transport.physically_closed);
        assert!(transport.socket.is_none());
        assert!(transport.read.is_none());
        assert!(transport.write.is_none());
        let mut wire = Vec::new();
        peer.read_to_end(&mut wire).unwrap();
        assert!(wire.starts_with(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"));
    }
}
