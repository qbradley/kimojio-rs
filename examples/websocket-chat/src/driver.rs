//! Native tasks exist only at the outer executor boundary.
use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::io::{IoSlice, Write};
use std::mem::size_of;
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, Instant};

use kimojio::{AsyncEvent, CancellationToken, Errno, OwnedFd, operations};
use kimojio_fsm_http1::{IoError, IoErrorKind, Tick};
use rustix::net::{AddressFamily, SocketType, ipproto, sockopt};

use crate::{
    chat::{self, Chat, Completion, Identity, Io},
    hub::{self, ClientId},
    native,
};

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub chat: chat::Config,
    pub run_for: Duration,
    pub shutdown_grace: Duration,
    pub send_buffer_bytes: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:0".parse().unwrap(),
            chat: chat::Config::default(),
            run_for: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(1),
            send_buffer_bytes: 65536,
        }
    }
}

#[allow(
    clippy::large_enum_variant,
    reason = "fixed mailbox slots are charged once instead of allocating every completion"
)]
enum Event {
    Accepted(Result<OwnedFd, Errno>),
    Io {
        client: ClientId,
        identity: Identity,
        completion: Completion,
    },
    Timer(Result<(), Errno>),
}
struct Mailbox {
    queue: RefCell<VecDeque<Event>>,
    wake: AsyncEvent,
}
impl Mailbox {
    fn publish(&self, event: Event) {
        {
            let mut queue = self.queue.borrow_mut();
            assert!(
                queue.len() < queue.capacity(),
                "one completion per reserved worker slot"
            );
            queue.push_back(event);
        }
        self.wake.set();
    }
}
struct Worker {
    cancellation: Rc<CancellationToken>,
    task: operations::TaskHandle<()>,
}
struct IoWorker {
    identity: Identity,
    worker: Worker,
}
struct Socket {
    client: ClientId,
    fd: Option<Rc<OwnedFd>>,
    workers: [Option<IoWorker>; 2],
}

enum Action {
    Io(ClientId, Io),
    Cancel(ClientId, Identity),
    Deadline,
    Retired(ClientId),
    Yield,
}
struct RootPorts;
impl chat::Ports for RootPorts {
    type Output = Action;
    fn io(&mut self, client: ClientId, operation: Io) -> Option<Action> {
        Some(Action::Io(client, operation))
    }
    fn cancel(&mut self, client: ClientId, target: Identity) -> Option<Action> {
        Some(Action::Cancel(client, target))
    }
    fn deadline_changed(&mut self, _: Option<Tick>) -> Option<Action> {
        Some(Action::Deadline)
    }
    fn retired(&mut self, client: ClientId) -> Option<Action> {
        Some(Action::Retired(client))
    }
    fn yield_turn(&mut self) -> Option<Action> {
        Some(Action::Yield)
    }
}

fn io_error(error: Errno) -> IoError {
    IoError {
        code: Some(error.raw_os_error()),
        kind: match error {
            Errno::AGAIN => IoErrorKind::WouldBlock,
            Errno::INTR => IoErrorKind::Interrupted,
            Errno::CANCELED => IoErrorKind::Cancelled,
            Errno::CONNRESET | Errno::PIPE | Errno::NOTCONN => IoErrorKind::Reset,
            _ => IoErrorKind::Other,
        },
    }
}

async fn close_owned(fd: Rc<OwnedFd>) -> Result<(), Errno> {
    let fd = Rc::try_unwrap(fd).map_err(|_| Errno::BUSY)?;
    operations::close(fd).await
}

async fn execute(
    fd: Rc<OwnedFd>,
    client: ClientId,
    operation: Io,
    cancellation: Rc<CancellationToken>,
    mailbox: Rc<Mailbox>,
) {
    let identity = operation.identity();
    let completion = match operation {
        Io::HttpRead(mut op) => {
            let result = native::read_once(&*fd, op.bytes_mut(), &cancellation)
                .await
                .map_err(io_error);
            Completion::HttpRead(op.complete(result))
        }
        Io::WsRead(mut op) => {
            let result = native::read_once(&*fd, op.bytes_mut(), &cancellation)
                .await
                .map_err(io_error);
            Completion::WsRead(op.complete(result))
        }
        Io::HttpWrite(op) => {
            let slices = op.slices().map(IoSlice::new);
            let result = native::write_once(&*fd, &slices, &cancellation)
                .await
                .map_err(io_error);
            Completion::HttpWrite(op.complete(result))
        }
        Io::WsWrite(op) => {
            let slices = op.slices().map(IoSlice::new);
            let result = native::write_once(&*fd, &slices, &cancellation)
                .await
                .map_err(io_error);
            Completion::WsWrite(op.complete(result))
        }
        // These sockets do not have O_NONBLOCK. io_uring waits for socket
        // readiness internally. Kimojio has no raw poll operation: if a future
        // backend unexpectedly requests readiness, fail explicitly, not by
        // claiming readiness or busy-polling a retry.
        Io::HttpReadiness(op) => {
            Completion::HttpReadiness(op.complete(Err(io_error(Errno::NOSYS))))
        }
        Io::WsReadiness(op) => Completion::WsReadiness(op.complete(Err(io_error(Errno::NOSYS)))),
        Io::HttpClose(op) => {
            Completion::HttpClose(op.complete(close_owned(fd).await.map_err(io_error)))
        }
        Io::WsClose(op) => {
            Completion::WsClose(op.complete(close_owned(fd).await.map_err(io_error)))
        }
    };
    mailbox.publish(Event::Io {
        client,
        identity,
        completion,
    });
}

async fn accept(listener: Rc<OwnedFd>, cancellation: Rc<CancellationToken>, mailbox: Rc<Mailbox>) {
    let result = if cancellation.is_cancelled() {
        Err(Errno::CANCELED)
    } else {
        native::settle_one(operations::accept(&*listener), &cancellation, |op| {
            op.cancel()
        })
        .await
    };
    mailbox.publish(Event::Accepted(result));
}
async fn timer(
    origin: Instant,
    at: Tick,
    cancellation: Rc<CancellationToken>,
    mailbox: Rc<Mailbox>,
) {
    let result = if !cancellation.is_cancelled() {
        let original = operations::sleep_until(origin + Duration::from_nanos(at.0));
        native::settle_one(original, &cancellation, |op| Pin::new(op).cancel()).await
    } else {
        Err(Errno::CANCELED)
    };
    mailbox.publish(Event::Timer(result));
}

type Execute<F> = fn(Rc<OwnedFd>, ClientId, Io, Rc<CancellationToken>, Rc<Mailbox>) -> F;
fn io_future_bytes<F: Future<Output = ()>>(_: Execute<F>) -> usize {
    size_of::<F>()
}
fn accept_future_bytes<F: Future<Output = ()>>(
    _: fn(Rc<OwnedFd>, Rc<CancellationToken>, Rc<Mailbox>) -> F,
) -> usize {
    size_of::<F>()
}
fn timer_future_bytes<F: Future<Output = ()>>(
    _: fn(Instant, Tick, Rc<CancellationToken>, Rc<Mailbox>) -> F,
) -> usize {
    size_of::<F>()
}
fn clock(origin: Instant) -> Tick {
    Tick(origin.elapsed().as_nanos().min(u64::MAX as u128) as u64)
}
fn add_duration(tick: Tick, duration: Duration) -> Tick {
    Tick(
        tick.0
            .saturating_add(duration.as_nanos().min(u64::MAX as u128) as u64),
    )
}

/// Runs one bounded root scheduler. Raw workers never execute application or
/// protocol transitions, and all their original futures settle before exit.
pub async fn run(mut config: Config) -> Result<hub::Stats, String> {
    let maximum = config.chat.hub.max_clients;
    if maximum == 0
        || maximum > 4096
        || config.run_for.is_zero()
        || config.run_for > Duration::from_secs(86400)
        || config.shutdown_grace > Duration::from_secs(60)
        || config.send_buffer_bytes == 0
    {
        return Err("invalid runtime or admission configuration".into());
    }
    let queue = VecDeque::with_capacity(2 * maximum + 2);
    let rc_counts = 2 * size_of::<usize>();
    config.chat.hub.external_fixed_bytes = queue.capacity() * size_of::<Event>()
        + size_of::<Mailbox>()
        + rc_counts
        + maximum * size_of::<Option<Socket>>()
        + accept_future_bytes(accept)
        + timer_future_bytes(timer)
        + 2 * (size_of::<CancellationToken>() + rc_counts)
        + size_of::<OwnedFd>()
        + rc_counts;
    config.chat.hub.external_bytes_per_client = 2
        * (io_future_bytes(execute) + size_of::<CancellationToken>() + rc_counts)
        + size_of::<OwnedFd>()
        + rc_counts;
    let mut chat =
        Chat::new(config.chat.clone()).map_err(|e| format!("invalid chat configuration: {e:?}"))?;
    let mut sockets: Box<[Option<Socket>]> = hub::empty_slots(maximum);
    let mailbox = Rc::new(Mailbox {
        queue: RefCell::new(queue),
        wake: AsyncEvent::new(),
    });
    let listener = operations::socket(
        if config.bind.is_ipv4() {
            AddressFamily::INET
        } else {
            AddressFamily::INET6
        },
        SocketType::STREAM,
        Some(ipproto::TCP),
    )
    .await
    .map_err(|e| format!("socket: {e}"))?;
    sockopt::set_socket_reuseaddr(&listener, true).map_err(|e| format!("reuseaddr: {e}"))?;
    operations::bind(&listener, &config.bind).map_err(|e| format!("bind: {e}"))?;
    operations::listen(&listener, maximum.min(128) as i32).map_err(|e| format!("listen: {e}"))?;
    let address = SocketAddr::try_from(
        rustix::net::getsockname(&listener).map_err(|e| format!("getsockname: {e}"))?,
    )
    .map_err(|_| "listener did not return an IP address")?;
    let listener = Rc::new(listener);
    println!("LISTEN {address}");
    std::io::stdout()
        .flush()
        .map_err(|e| format!("readiness output: {e}"))?;
    let origin = Instant::now();
    let stop_at = add_duration(Tick(0), config.run_for);
    let mut stopping = false;
    let mut abort_at = None;
    let mut aborted = false;
    let mut accept_worker: Option<Worker> = None;
    let mut timer_worker: Option<(Tick, Worker)> = None;

    loop {
        let now = clock(origin);
        chat.expire_due(now);
        if !stopping && now >= stop_at {
            stopping = true;
            abort_at = Some(add_duration(now, config.shutdown_grace));
            chat.shutdown(false);
            if let Some(worker) = &accept_worker {
                worker.cancellation.cancel();
            }
        }
        if !aborted && abort_at.is_some_and(|at| now >= at) {
            aborted = true;
            chat.shutdown(true);
        }
        let mut work = 0;
        while work < 64 {
            let event = mailbox.queue.borrow_mut().pop_front();
            let Some(event) = event else { break };
            work += 1;
            match event {
                Event::Accepted(result) => {
                    accept_worker
                        .take()
                        .expect("one accept worker")
                        .task
                        .await
                        .map_err(|e| format!("accept worker: {e:?}"))?;
                    match result {
                        Ok(fd) if !stopping => {
                            if let Err(error) =
                                sockopt::set_socket_send_buffer_size(&fd, config.send_buffer_bytes)
                            {
                                operations::close(fd)
                                    .await
                                    .map_err(|e| format!("close rejected socket: {e}"))?;
                                eprintln!("socket send buffer: {error}");
                            } else {
                                match chat.admit() {
                                    Ok(client) => {
                                        sockets[client.slot()] = Some(Socket {
                                            client,
                                            fd: Some(Rc::new(fd)),
                                            workers: [None, None],
                                        })
                                    }
                                    Err(_) => {
                                        operations::close(fd)
                                            .await
                                            .map_err(|e| format!("close admission: {e}"))?;
                                    }
                                }
                            }
                        }
                        Ok(fd) => {
                            operations::close(fd)
                                .await
                                .map_err(|e| format!("close late accept: {e}"))?;
                        }
                        Err(Errno::CANCELED | Errno::INTR) => {}
                        Err(error) => {
                            eprintln!("accept: {error}; stopping");
                            stopping = true;
                            aborted = true;
                            chat.shutdown(true);
                        }
                    }
                }
                Event::Timer(result) => {
                    timer_worker
                        .take()
                        .expect("one timer worker")
                        .1
                        .task
                        .await
                        .map_err(|e| format!("timer worker: {e:?}"))?;
                    if let Err(error) = result
                        && error != Errno::CANCELED
                    {
                        eprintln!("timer: {error}; stopping");
                        stopping = true;
                        aborted = true;
                        chat.shutdown(true);
                        if let Some(worker) = &accept_worker {
                            worker.cancellation.cancel();
                        }
                    }
                }
                Event::Io {
                    client,
                    identity,
                    completion,
                } => {
                    let socket = sockets[client.slot()]
                        .as_mut()
                        .expect("original operation pins socket slot");
                    assert_eq!(socket.client, client);
                    let lane = socket
                        .workers
                        .iter()
                        .position(|slot| {
                            slot.as_ref()
                                .is_some_and(|worker| worker.identity == identity)
                        })
                        .expect("phase-tagged original identity");
                    socket.workers[lane]
                        .take()
                        .unwrap()
                        .worker
                        .task
                        .await
                        .map_err(|e| format!("I/O worker: {e:?}"))?;
                    chat.observe_time(clock(origin));
                    chat.complete(client, completion)
                        .expect("original completion routed to its live protocol owner");
                }
            }
        }
        while work < 128 {
            let Some(action) = chat.next(&mut RootPorts) else {
                break;
            };
            work += 1;
            match action {
                Action::Io(client, operation) => {
                    let socket = sockets[client.slot()]
                        .as_mut()
                        .expect("admitted client owns socket");
                    let lane = operation.lane();
                    assert!(socket.workers[lane].is_none());
                    let identity = operation.identity();
                    let fd = if operation.is_close() {
                        assert!(socket.workers.iter().all(Option::is_none));
                        socket.fd.take().expect("exactly one transport close")
                    } else {
                        socket.fd.as_ref().unwrap().clone()
                    };
                    let cancellation = Rc::new(CancellationToken::new());
                    let task = operations::spawn_task(execute(
                        fd,
                        client,
                        operation,
                        cancellation.clone(),
                        mailbox.clone(),
                    ));
                    socket.workers[lane] = Some(IoWorker {
                        identity,
                        worker: Worker { cancellation, task },
                    });
                }
                Action::Cancel(client, identity) => {
                    if let Some(socket) = &sockets[client.slot()] {
                        assert_eq!(socket.client, client);
                        for worker in socket
                            .workers
                            .iter()
                            .flatten()
                            .filter(|worker| worker.identity == identity)
                        {
                            worker.worker.cancellation.cancel();
                        }
                    }
                }
                Action::Retired(client) => {
                    let socket = sockets[client.slot()].take().expect("retired socket");
                    assert_eq!(socket.client, client);
                    assert!(socket.fd.is_none() && socket.workers.iter().all(Option::is_none));
                }
                Action::Deadline => {}
                Action::Yield => {
                    work = 128;
                }
            }
        }
        if !stopping && accept_worker.is_none() && chat.can_admit() {
            let cancellation = Rc::new(CancellationToken::new());
            let task = operations::spawn_task(accept(
                listener.clone(),
                cancellation.clone(),
                mailbox.clone(),
            ));
            accept_worker = Some(Worker { cancellation, task });
        }
        let settled = stopping && chat.stats().resident_clients == 0 && accept_worker.is_none();
        let target = if settled {
            None
        } else if !stopping {
            Some(stop_at)
        } else if !aborted {
            abort_at
        } else {
            None
        };
        let target = target.into_iter().chain(chat.deadline()).min();
        if let Some((at, worker)) = &timer_worker {
            if Some(*at) != target {
                worker.cancellation.cancel();
            }
        } else if let Some(at) = target {
            let cancellation = Rc::new(CancellationToken::new());
            let task =
                operations::spawn_task(timer(origin, at, cancellation.clone(), mailbox.clone()));
            timer_worker = Some((at, Worker { cancellation, task }));
        }
        if stopping && chat.stats().resident_clients == 0 && accept_worker.is_none() {
            if let Some((_, worker)) = &timer_worker {
                worker.cancellation.cancel();
            }
            if timer_worker.is_none() {
                break;
            }
        }
        if work >= 128 {
            operations::yield_io().await;
            continue;
        }
        mailbox.wake.reset();
        if !mailbox.queue.borrow().is_empty() {
            continue;
        }
        mailbox
            .wake
            .wait()
            .await
            .map_err(|e| format!("root wake: {e:?}"))?;
    }
    close_owned(listener)
        .await
        .map_err(|e| format!("close listener: {e}"))?;
    let stats = chat.stats();
    eprintln!(
        "STOP clients={} used={} peak={}",
        stats.resident_clients, stats.used_bytes, stats.peak_bytes
    );
    Ok(stats)
}
