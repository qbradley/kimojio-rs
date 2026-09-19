//! The only runtime layer: bounded readiness queue, deadlines, and real SQ/CQ I/O.
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::CString;
use std::io::{self, Write};
use std::net::SocketAddrV4;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use kimojio_fsm_http1 as http;
use rustix::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use rustix::fs::OFlags;
use rustix::net::{self, AddressFamily, SendFlags, SocketFlags, SocketType};
use rustix_uring::{Errno, opcode, squeue::Entry, types::Fd};

use crate::app::{self, Buffer};
use crate::composite::{self, Service};
use crate::file::FileOperation;
use crate::ring::{Event, Operation, Ring};

pub struct Options {
    pub bind: SocketAddrV4,
    pub root: PathBuf,
    pub max_connections: usize,
    pub timeout: Duration,
    pub stop_after: Option<usize>,
}

#[expect(
    clippy::large_enum_variant,
    reason = "The ring boxes each operation once; stat storage shares that allocation."
)]
enum Io {
    Socket {
        descriptor: Option<OwnedFd>,
    },
    Root {
        path: CString,
        descriptor: Option<OwnedFd>,
    },
    Accept {
        listener: Rc<OwnedFd>,
        descriptor: Option<OwnedFd>,
    },
    Read {
        owner: u64,
        socket: Rc<OwnedFd>,
        op: http::ReadOp<Buffer>,
    },
    Write {
        owner: u64,
        socket: Rc<OwnedFd>,
        op: http::WriteOp<Buffer>,
    },
    Ready {
        owner: u64,
        socket: Rc<OwnedFd>,
        op: http::ReadinessOp,
    },
    Close {
        owner: u64,
        descriptor: Option<OwnedFd>,
        op: http::CloseOp,
    },
    Retire {
        descriptor: Option<OwnedFd>,
    },
    File(FileOperation),
}

// Submitted Io values live in stable Boxes. Network operations retain the
// descriptor and owned HTTP lease until their original CQE. FileOperation has
// the same contract. Descriptor-producing CQEs become OwnedFd before routing.
unsafe impl Operation for Io {
    unsafe fn entry(&mut self) -> Entry {
        match self {
            Self::Socket { .. } => opcode::Socket::new(
                AddressFamily::INET.as_raw() as i32,
                (SocketType::STREAM.as_raw() | SocketFlags::CLOEXEC.bits()) as i32,
                0,
            )
            .build(),
            Self::Root { path, .. } => opcode::OpenAt::new(Fd(-100), path.as_ptr())
                .flags(OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC)
                .build(),
            Self::Accept { listener, .. } => opcode::Accept::new(
                Fd(listener.as_raw_fd()),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
            .flags(SocketFlags::CLOEXEC)
            .build(),
            Self::Read { socket, op, .. } => {
                let bytes = op.bytes_mut();
                opcode::Recv::new(
                    Fd(socket.as_raw_fd()),
                    bytes.as_mut_ptr(),
                    bytes.len() as u32,
                )
                .build()
            }
            Self::Write { socket, op, .. } => {
                // The core owns the wire cursor. Sending one nonempty segment
                // avoids temporary iovecs and returns exact partial progress.
                let bytes = op
                    .slices()
                    .into_iter()
                    .find(|bytes| !bytes.is_empty())
                    .unwrap();
                opcode::Send::new(Fd(socket.as_raw_fd()), bytes.as_ptr(), bytes.len() as u32)
                    .flags(SendFlags::NOSIGNAL)
                    .build()
            }
            Self::Ready { socket, op, .. } => opcode::PollAdd::new(
                Fd(socket.as_raw_fd()),
                match op.direction {
                    http::Direction::Read => 1,
                    http::Direction::Write => 4,
                },
            )
            .build(),
            Self::Close { descriptor, .. } => {
                opcode::Close::new(Fd(descriptor.take().unwrap().into_raw_fd())).build()
            }
            Self::Retire { descriptor } => {
                opcode::Close::new(Fd(descriptor.take().unwrap().into_raw_fd())).build()
            }
            // The containing Io has the same stable storage and lifetime.
            Self::File(op) => unsafe { op.entry() },
        }
    }
    unsafe fn completed(&mut self, result: Result<u32, Errno>) {
        match self {
            Self::Socket { descriptor }
            | Self::Root { descriptor, .. }
            | Self::Accept { descriptor, .. } => {
                if let Ok(fd) = result {
                    // A successful socket/open/accept CQE transfers ownership.
                    *descriptor = Some(unsafe { OwnedFd::from_raw_fd(fd as i32) });
                }
            }
            // This is the authentic original CQE for the contained operation.
            Self::File(op) => unsafe { op.completed(result) },
            _ => {}
        }
    }
    fn cancelable(&self) -> bool {
        match self {
            Self::Close { .. } | Self::Retire { .. } => false,
            Self::File(op) => op.cancelable(),
            _ => true,
        }
    }
}

struct Connection {
    service: Service,
    socket: Option<Rc<OwnedFd>>,
    file: Option<OwnedFd>,
    file_key: u64,
    file_operation: Option<u64>,
    operations: HashMap<http::OperationId, u64>,
    deadline: Option<http::Deadline>,
    queued: bool,
}

fn submit(ring: &mut Ring<Io>, operation: Io) -> u64 {
    ring.submit(operation)
        .unwrap_or_else(|_| panic!("reserved root operation capacity exhausted"))
}

fn startup(ring: &mut Ring<Io>, operation: Io) -> io::Result<OwnedFd> {
    submit(ring, operation);
    loop {
        for event in ring.poll(None).map_err(io::Error::from)? {
            if let Event::Completed {
                operation, result, ..
            } = event
            {
                result.map_err(io::Error::from)?;
                match *operation {
                    Io::Socket { descriptor } | Io::Root { descriptor, .. } => {
                        return Ok(descriptor.unwrap());
                    }
                    _ => unreachable!(),
                }
            }
        }
    }
}

fn enqueue(connections: &mut BTreeMap<u64, Connection>, ready: &mut VecDeque<u64>, id: u64) {
    if let Some(connection) = connections.get_mut(&id)
        && !connection.queued
    {
        connection.queued = true;
        ready.push_back(id);
    }
}

fn tick(start: Instant) -> http::Tick {
    http::Tick(start.elapsed().as_nanos().min(u64::MAX as u128) as u64)
}

/// Runs until `stop_after` completed exchanges have settled, or a root I/O fails.
/// Bind/listen are setup-only rustix calls; all connection/file I/O uses the ring.
pub fn run(options: Options) -> io::Result<()> {
    if !(1..=256).contains(&options.max_connections) || options.timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid connection limit or timeout",
        ));
    }
    let mut ring = Ring::new((options.max_connections * 4 + 8).next_power_of_two() as u32)
        .map_err(io::Error::from)?;
    let path = CString::new(options.root.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "root contains NUL"))?;
    let root = Rc::new(startup(
        &mut ring,
        Io::Root {
            path,
            descriptor: None,
        },
    )?);
    let listener = Rc::new(startup(&mut ring, Io::Socket { descriptor: None })?);
    net::bind(&*listener, &options.bind)?;
    net::listen(&*listener, options.max_connections as i32)?;
    let address = SocketAddrV4::try_from(net::getsockname(&*listener)?)?;
    println!("LISTEN {address}");
    io::stdout().flush()?;
    let start = Instant::now();
    let mut connections = BTreeMap::<u64, Connection>::new();
    let mut ready = VecDeque::new();
    let mut deadlines = BTreeMap::<(http::Tick, u64), http::Deadline>::new();
    let mut next_connection = 1_u64;
    let mut accept = None;
    let mut finished = 0;
    let mut stopping = false;
    let timeout = options.timeout.as_nanos().min(u64::MAX as u128) as u64;
    let config = http::Config {
        max_buffer_bytes: 32 * 1024,
        max_body_bytes: u64::MAX,
        head_timeout_ns: Some(timeout),
        body_timeout_ns: Some(timeout),
        idle_timeout_ns: Some(timeout),
        ..http::Config::default()
    };

    loop {
        if !stopping && accept.is_none() && connections.len() < options.max_connections {
            accept = Some(submit(
                &mut ring,
                Io::Accept {
                    listener: listener.clone(),
                    descriptor: None,
                },
            ));
        }
        // A fixed batch budget allows CQEs and deadlines to run even when
        // several pipelined peers remain locally runnable.
        for _ in 0..256 {
            let Some(id) = ready.pop_front() else { break };
            let Some(connection) = connections.get_mut(&id) else {
                continue;
            };
            connection.queued = false;
            connection.service.observe_time(tick(start));
            let yielded = {
                let mut ports = RootPorts {
                    id,
                    ring: &mut ring,
                    root: &root,
                    socket: &mut connection.socket,
                    file: &mut connection.file,
                    file_key: connection.file_key,
                    file_operation: &mut connection.file_operation,
                    operations: &mut connection.operations,
                    deadline: &mut connection.deadline,
                    deadlines: &mut deadlines,
                    finished: &mut finished,
                };
                connection.service.next(&mut ports).is_some()
            };
            if connection.service.settled() {
                assert!(connection.file.is_none() && connection.socket.is_none());
                assert!(connection.operations.is_empty());
                if let Some(deadline) = connection.deadline.take() {
                    deadlines.remove(&(deadline.at, id));
                }
                connections.remove(&id);
            } else if yielded {
                enqueue(&mut connections, &mut ready, id);
            }
            if !stopping && options.stop_after.is_some_and(|limit| finished >= limit) {
                stopping = true;
                if let Some(token) = accept {
                    ring.cancel(token);
                }
                let ids: Vec<_> = connections.keys().copied().collect();
                for id in ids {
                    connections
                        .get_mut(&id)
                        .unwrap()
                        .service
                        .shutdown(http::ShutdownMode::Graceful);
                    enqueue(&mut connections, &mut ready, id);
                }
            }
        }
        let now = tick(start);
        while let Some((&(at, id), _)) = deadlines.first_key_value() {
            if at > now {
                break;
            }
            let (_, deadline) = deadlines.pop_first().unwrap();
            if let Some(connection) = connections.get_mut(&id) {
                connection.deadline = None;
                connection.service.expire(deadline, now);
                enqueue(&mut connections, &mut ready, id);
            }
        }
        if stopping && connections.is_empty() && ring.is_empty() {
            break;
        }
        let wait = if !ready.is_empty() {
            Some(Duration::ZERO)
        } else {
            deadlines
                .first_key_value()
                .map(|(&(at, _), _)| Duration::from_nanos(at.0.saturating_sub(tick(start).0)))
        };
        for event in ring.poll(wait).map_err(io::Error::from)? {
            let Event::Completed {
                token,
                operation,
                result,
            } = event
            else {
                // Cancel CQEs settle cancellation requests, never original I/O.
                continue;
            };
            match *operation {
                Io::Accept { descriptor, .. } => {
                    accept = None;
                    if let Some(socket) = descriptor {
                        if stopping {
                            submit(
                                &mut ring,
                                Io::Retire {
                                    descriptor: Some(socket),
                                },
                            );
                            continue;
                        }
                        let id = next_connection;
                        next_connection = next_connection
                            .checked_add(1)
                            .expect("connection identity exhausted");
                        let service = Service::new(id, config.clone(), tick(start))
                            .map_err(io::Error::other)?;
                        connections.insert(
                            id,
                            Connection {
                                service,
                                socket: Some(Rc::new(socket)),
                                file: None,
                                file_key: 0,
                                file_operation: None,
                                operations: HashMap::new(),
                                deadline: None,
                                queued: false,
                            },
                        );
                        enqueue(&mut connections, &mut ready, id);
                    } else if result != Err(Errno::CANCELED) {
                        result.map_err(io::Error::from)?;
                    }
                }
                Io::Read { owner, op, .. } => {
                    let connection = connections.get_mut(&owner).unwrap();
                    connection.operations.remove(&op.id());
                    connection
                        .service
                        .complete_read(op.complete(io_count(result)));
                    enqueue(&mut connections, &mut ready, owner);
                }
                Io::Write { owner, op, .. } => {
                    let connection = connections.get_mut(&owner).unwrap();
                    connection.operations.remove(&op.id());
                    connection
                        .service
                        .complete_write(op.complete(io_count(result)));
                    enqueue(&mut connections, &mut ready, owner);
                }
                Io::Ready { owner, op, .. } => {
                    let connection = connections.get_mut(&owner).unwrap();
                    connection.operations.remove(&op.id());
                    connection
                        .service
                        .complete_readiness(op.complete(io_count(result).map(|_| ())));
                    enqueue(&mut connections, &mut ready, owner);
                }
                Io::Close { owner, op, .. } => {
                    let connection = connections.get_mut(&owner).unwrap();
                    connection.operations.remove(&op.id());
                    connection
                        .service
                        .complete_close(op.complete(io_count(result).map(|_| ())));
                    enqueue(&mut connections, &mut ready, owner);
                }
                Io::File(operation) => {
                    let finished = operation.finish(token);
                    let owner = finished.owner;
                    let connection = connections.get_mut(&owner).unwrap();
                    assert_eq!(connection.file_operation.take(), Some(token));
                    if let app::Completion::Open {
                        result: Ok(file), ..
                    } = &finished.completion
                    {
                        connection.file_key = file.0;
                    }
                    connection.file = finished.descriptor;
                    connection.service.complete_file(finished.completion);
                    enqueue(&mut connections, &mut ready, owner);
                }
                Io::Retire { .. } => {
                    result.map_err(io::Error::from)?;
                }
                _ => unreachable!("startup operations already settled"),
            }
        }
    }
    submit(
        &mut ring,
        Io::Retire {
            descriptor: Some(Rc::try_unwrap(listener).expect("accept settled")),
        },
    );
    submit(
        &mut ring,
        Io::Retire {
            descriptor: Some(Rc::try_unwrap(root).expect("file operations settled")),
        },
    );
    while !ring.is_empty() {
        for event in ring.poll(None).map_err(io::Error::from)? {
            if let Event::Completed { result, .. } = event {
                result.map_err(io::Error::from)?;
            }
        }
    }
    Ok(())
}

struct RootPorts<'a> {
    id: u64,
    ring: &'a mut Ring<Io>,
    root: &'a Rc<OwnedFd>,
    socket: &'a mut Option<Rc<OwnedFd>>,
    file: &'a mut Option<OwnedFd>,
    file_key: u64,
    file_operation: &'a mut Option<u64>,
    operations: &'a mut HashMap<http::OperationId, u64>,
    deadline: &'a mut Option<http::Deadline>,
    deadlines: &'a mut BTreeMap<(http::Tick, u64), http::Deadline>,
    finished: &'a mut usize,
}

impl RootPorts<'_> {
    fn network(&mut self, id: http::OperationId, operation: Io) {
        let token = submit(self.ring, operation);
        assert!(self.operations.insert(id, token).is_none());
    }
    fn take_file(&mut self, key: app::File) -> OwnedFd {
        assert_eq!(key.0, self.file_key);
        self.file.take().expect("exclusive file operation")
    }

    fn file(&mut self, operation: FileOperation) {
        let token = submit(self.ring, Io::File(operation));
        assert!(self.file_operation.replace(token).is_none());
    }
}

impl composite::Ports for RootPorts<'_> {
    type Output = ();
    fn read(&mut self, op: http::ReadOp<Buffer>) -> Option<()> {
        self.network(
            op.id(),
            Io::Read {
                owner: self.id,
                socket: self.socket.as_ref().unwrap().clone(),
                op,
            },
        );
        None
    }
    fn write(&mut self, op: http::WriteOp<Buffer>) -> Option<()> {
        self.network(
            op.id(),
            Io::Write {
                owner: self.id,
                socket: self.socket.as_ref().unwrap().clone(),
                op,
            },
        );
        None
    }
    fn readiness(&mut self, op: http::ReadinessOp) -> Option<()> {
        self.network(
            op.id(),
            Io::Ready {
                owner: self.id,
                socket: self.socket.as_ref().unwrap().clone(),
                op,
            },
        );
        None
    }
    fn cancel(&mut self, op: http::CancelOp) -> Option<()> {
        if let Some(token) = self.operations.get(&op.target) {
            self.ring.cancel(*token);
        }
        None
    }
    fn close(&mut self, op: http::CloseOp) -> Option<()> {
        let descriptor = Rc::try_unwrap(self.socket.take().unwrap())
            .expect("original socket operations settled");
        self.network(
            op.id(),
            Io::Close {
                owner: self.id,
                descriptor: Some(descriptor),
                op,
            },
        );
        None
    }
    fn open(&mut self, op: app::Open) -> Option<()> {
        self.file(FileOperation::open(self.id, op, self.root.clone()));
        None
    }
    fn stat(&mut self, op: app::Stat) -> Option<()> {
        let descriptor = self.take_file(op.file);
        self.file(FileOperation::stat(self.id, op, descriptor));
        None
    }
    fn file_read(&mut self, op: app::Read) -> Option<()> {
        let descriptor = self.take_file(op.file);
        self.file(FileOperation::read(self.id, op, descriptor));
        None
    }
    fn file_close(&mut self, op: app::Close) -> Option<()> {
        let descriptor = self.take_file(op.file);
        self.file(FileOperation::close(self.id, op, descriptor));
        None
    }
    fn deadline_changed(&mut self, deadline: Option<http::Deadline>) -> Option<()> {
        if let Some(previous) = self.deadline.take() {
            self.deadlines.remove(&(previous.at, self.id));
        }
        *self.deadline = deadline;
        if let Some(deadline) = deadline {
            self.deadlines.insert((deadline.at, self.id), deadline);
        }
        None
    }
    fn exchange_finished(&mut self) -> Option<()> {
        *self.finished += 1;
        Some(())
    }
    fn closed(&mut self, result: http::ConnectionResult) -> Option<()> {
        if let Err(error) = result {
            eprintln!("connection {}: {error:?}", self.id);
        }
        if let Some(token) = *self.file_operation {
            self.ring.cancel(token);
        }
        None
    }
    fn yield_turn(&mut self) -> Option<()> {
        Some(())
    }
}

fn io_count(result: Result<u32, Errno>) -> http::IoResult<usize> {
    result
        .map(|count| count as usize)
        .map_err(|error| http::IoError {
            kind: match error {
                Errno::AGAIN => http::IoErrorKind::WouldBlock,
                Errno::INTR => http::IoErrorKind::Interrupted,
                Errno::CANCELED => http::IoErrorKind::Cancelled,
                Errno::CONNRESET | Errno::PIPE => http::IoErrorKind::Reset,
                _ => http::IoErrorKind::Other,
            },
            code: Some(error.raw_os_error()),
        })
}
