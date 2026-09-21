use futures::{FutureExt, future::LocalBoxFuture};
use kimojio::{OwnedFdStream, SplittableStream, operations};
use kimojio_http1::{
    Client, Config, ConnectionId, Error, Observation, OutgoingBody, connect, connect_native,
    http::Response, serve_connection, serve_connection_native,
};

fn config(slot: u64, observation: &Observation) -> Config {
    let mut config = Config::new(ConnectionId {
        slot,
        generation: 1,
    });
    assert!(config.observation.is_none());
    config.observation = Some(observation.clone());
    config.protocol.head_timeout_ns = None;
    config.protocol.idle_timeout_ns = None;
    config
}

fn driver(
    native: bool,
    server: bool,
    fd: kimojio::OwnedFd,
    config: Config,
) -> (Option<Client>, LocalBoxFuture<'static, Result<(), Error>>) {
    if server {
        let handler = |_| async { Ok(Response::new(OutgoingBody::empty())) };
        let driver = if native {
            serve_connection_native(fd, config, handler).boxed_local()
        } else {
            serve_connection(OwnedFdStream::new(fd), config, handler).boxed_local()
        };
        (None, driver)
    } else if native {
        let (client, driver) = connect_native(fd, config);
        (Some(client), driver.run().boxed_local())
    } else {
        let (client, driver) = connect(OwnedFdStream::new(fd), config);
        (Some(client), driver.run().boxed_local())
    }
}

#[kimojio::test]
async fn cloned_config_cannot_bind_a_second_driver_even_before_first_poll() {
    for native in [false, true] {
        for server in [false, true] {
            let observation = Observation::new();
            let config = config(1, &observation);
            let (first, _first_peer) = kimojio::pipe::bipipe();
            let (_client, first) = driver(native, server, first, config.clone());
            let (second, peer) = kimojio::pipe::bipipe();
            let (_second_client, second) = driver(native, server, second, config.clone());
            assert!(matches!(second.await, Err(Error::ObservationInUse)));
            assert_eq!(operations::read(&peer, &mut [0; 1]).await.unwrap(), 0);
            drop(first);
            let (third, _peer) = kimojio::pipe::bipipe();
            let (_client, third) = driver(native, server, third, config);
            assert!(matches!(third.await, Err(Error::ObservationInUse)));
            #[cfg(feature = "metrics")]
            assert!(matches!(observation.snapshot().await, Err(Error::Closed)));
        }
    }
}

struct SplitMustNotRun;

impl SplittableStream for SplitMustNotRun {
    type ReadStream = kimojio::OwnedFdStreamRead;
    type WriteStream = kimojio::OwnedFdStreamWrite;

    async fn split(self) -> Result<(Self::ReadStream, Self::WriteStream), kimojio::Errno> {
        panic!("duplicate observation must fail before transport split")
    }
}

#[kimojio::test]
async fn duplicate_binding_precedes_core_validation_and_transport_split() {
    let observation = Observation::new();
    let mut config = config(2, &observation);
    let (fd, _peer) = kimojio::pipe::bipipe();
    let (_client, first) = connect_native(fd, config.clone());
    config.protocol.max_headers = 0;
    let (_client, second) = connect(SplitMustNotRun, config);
    assert!(matches!(second.run().await, Err(Error::ObservationInUse)));
    drop(first);
}

#[kimojio::test]
async fn instrumented_core_logs_reach_native_and_generic_server_callbacks() {
    use kimojio_fsm_http1::{Failure, OperationKind};
    use kimojio_http1::{
        LogEvent, Shutdown, serve_connection_native_with_shutdown, serve_connection_with_shutdown,
    };
    use std::{cell::RefCell, rc::Rc};

    for native in [false, true] {
        let logs = Rc::new(RefCell::new(Vec::new()));
        let received = logs.clone();
        #[cfg(feature = "metrics")]
        let holder = Rc::new(RefCell::new(None::<Observation>));
        #[cfg(feature = "metrics")]
        let callback_handle = holder.clone();
        let observation = Observation::with_logger(move |id, now, event| {
            received.borrow_mut().push((id, now, event));
            #[cfg(feature = "metrics")]
            {
                let handle = callback_handle.borrow();
                let handle = handle.as_ref().unwrap();
                assert!(handle.final_snapshot().is_none());
                // Enqueue and cancel a query inside the actual core drive callback.
                assert!(handle.snapshot().now_or_never().is_none());
            }
        });
        #[cfg(feature = "metrics")]
        holder.borrow_mut().replace(observation.clone());
        let (fd, peer) = kimojio::pipe::bipipe();
        let shutdown = Shutdown::default();
        let handler = |_| async { panic!("no request was sent") };
        let mut server = if native {
            serve_connection_native_with_shutdown(
                fd,
                config(30, &observation),
                shutdown.clone(),
                handler,
            )
            .boxed_local()
        } else {
            serve_connection_with_shutdown(
                OwnedFdStream::new(fd),
                config(30, &observation),
                shutdown.clone(),
                handler,
            )
            .boxed_local()
        };
        assert!(futures::poll!(server.as_mut()).is_pending());
        assert!(logs.borrow().iter().any(|(_, _, event)| {
            matches!(event, LogEvent::OperationIssued(op) if op.kind() == OperationKind::Read)
        }));
        shutdown.abort();
        assert!(matches!(
            server.await,
            Err(Error::Protocol(Failure::Cancelled))
        ));
        #[cfg(feature = "metrics")]
        {
            assert_eq!(
                observation.snapshot().await.unwrap(),
                observation.final_snapshot().unwrap()
            );
            holder.borrow_mut().take();
        }
        assert_eq!(operations::read(&peer, &mut [0; 1]).await.unwrap(), 0);
        let logs = logs.borrow();
        assert!(logs.iter().all(|(id, _, _)| *id
            == ConnectionId {
                slot: 30,
                generation: 1
            }));
        assert!(logs.windows(2).all(|pair| pair[0].1 <= pair[1].1));
        let mut read = None;
        let sequence: Vec<_> = logs
            .iter()
            .filter_map(|(_, _, event)| match event {
                LogEvent::OperationIssued(op) if op.kind() == OperationKind::Read => {
                    assert!(read.replace(*op).is_none());
                    Some("read")
                }
                LogEvent::PrimaryFailure(Failure::Cancelled) => Some("failure"),
                LogEvent::CancellationRequested(op) => {
                    assert_eq!(Some(*op), read);
                    Some("cancel")
                }
                LogEvent::OperationIssued(op) if op.kind() == OperationKind::Close => Some("close"),
                LogEvent::Closed(Err(Failure::Cancelled)) => Some("closed"),
                LogEvent::DeadlineChanged(_) => None,
                _ => panic!("unexpected diagnostic event {event:?}"),
            })
            .collect();
        assert_eq!(sequence, ["read", "failure", "cancel", "close", "closed"]);
    }
}

#[cfg(feature = "metrics")]
mod metrics {
    use super::*;
    use kimojio_http1::{Counters, MetricsSnapshot, SnapshotPhase, http::Request};
    use std::{cell::Cell, rc::Rc, task::Poll};

    const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nhost: test\r\ncontent-length: 0\r\n\r\n";
    const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 6\r\n\r\nanswer";
    type Issued = Rc<Cell<(u64, u64, u64)>>;

    fn counted_observation() -> (Observation, Issued) {
        let issued = Rc::new(Cell::new((0, 0, 0)));
        let observation = {
            use kimojio_fsm_http1::OperationKind;
            use kimojio_http1::LogEvent;
            let issued = issued.clone();
            Observation::with_logger(move |_, _, event| {
                let (mut reads, mut writes, mut bodies) = issued.get();
                match event {
                    LogEvent::OperationIssued(op) if op.kind() == OperationKind::Read => reads += 1,
                    LogEvent::OperationIssued(op) if op.kind() == OperationKind::Write => {
                        writes += 1
                    }
                    LogEvent::BodyOffered { .. } => bodies += 1,
                    _ => {}
                }
                issued.set((reads, writes, bodies));
            })
        };
        (observation, issued)
    }

    fn assert_live(snapshot: MetricsSnapshot, server: bool) {
        assert_eq!(snapshot.phase, SnapshotPhase::Http);
        assert_eq!(snapshot.server, server);
        assert_eq!(snapshot.exchange.unwrap().connection(), snapshot.connection);
        assert_eq!(snapshot.read_outstanding, !server);
        assert!(!snapshot.write_outstanding);
        assert!(!snapshot.body_lease_outstanding);
        assert_eq!(snapshot.buffered_input_bytes, 0);
        assert_eq!(snapshot.failure, None);
        let mut expected = Counters::default();
        expected.exchanges_started = 1;
        if server {
            expected.read_completions = 1;
            expected.read_bytes = REQUEST.len() as u64;
        } else {
            expected.write_completions = 1;
            expected.written_bytes_lower_bound = REQUEST.len() as u64;
        }
        assert_eq!(snapshot.counters, expected);
    }

    #[kimojio::test]
    async fn snapshots_wake_idle_and_active_native_and_generic_drivers() {
        for native in [false, true] {
            let (client_observation, client_issued) = counted_observation();
            let (server_observation, server_issued) = counted_observation();
            let (fd, peer) = kimojio::pipe::bipipe();
            let (client, client_driver) =
                driver(native, false, fd, config(10, &client_observation));
            let mut client = client.unwrap();
            let (started, start) = kimojio::oneshot();
            let (allow, allowed) = kimojio::oneshot();
            let mut gate = Some((started, allowed));
            let handled = Rc::new(Cell::new(0));
            let count = handled.clone();
            let logger = server_observation.clone();
            let handler = move |request: Request<kimojio_http1::IncomingBody>| {
                assert_eq!(request.uri(), "/");
                count.set(count.get() + 1);
                let (started, allowed) = gate.take().unwrap();
                let observation = logger.clone();
                async move {
                    let snapshot = observation.snapshot().await?;
                    assert!(snapshot.server);
                    assert!(snapshot.exchange.is_some());
                    started.send(()).unwrap();
                    allowed.recv().await.unwrap();
                    Ok(Response::new(OutgoingBody::full(b"answer")))
                }
            };
            let server = if native {
                serve_connection_native(peer, config(11, &server_observation), handler)
                    .boxed_local()
            } else {
                serve_connection(
                    OwnedFdStream::new(peer),
                    config(11, &server_observation),
                    handler,
                )
                .boxed_local()
            };
            let app = async {
                operations::yield_cpu().await;
                for (observation, server, slot) in [
                    (&client_observation, false, 10),
                    (&server_observation, true, 11),
                ] {
                    let snapshot = observation.snapshot().await.unwrap();
                    assert_eq!(snapshot.connection.slot, slot);
                    assert_eq!(snapshot.server, server);
                    assert_eq!(snapshot.phase, SnapshotPhase::Http);
                    assert!(snapshot.exchange.is_none());
                    assert_eq!(snapshot.counters, Counters::default());
                    assert!(observation.final_snapshot().is_none());
                }
                let request = Request::builder()
                    .uri("/")
                    .header("host", "test")
                    .body(OutgoingBody::empty())
                    .unwrap();
                let inspect = async {
                    start.recv().await.unwrap();
                    let mut settled = None;
                    for _ in 0..64 {
                        let snapshot = client_observation.snapshot().await.unwrap();
                        if !snapshot.write_outstanding {
                            settled = Some(snapshot);
                            break;
                        }
                    }
                    let client_live = settled.expect("request write did not settle");
                    let server_live = server_observation.snapshot().await.unwrap();
                    assert_live(client_live, false);
                    assert_live(server_live, true);
                    let (duplicate, _peer) = kimojio::pipe::bipipe();
                    let (_client, duplicate) =
                        driver(native, false, duplicate, config(99, &client_observation));
                    assert!(matches!(duplicate.await, Err(Error::ObservationInUse)));
                    for _ in 0..16 {
                        let (client, server) = futures::join!(
                            client_observation.snapshot(),
                            server_observation.snapshot(),
                        );
                        let client = client.unwrap();
                        let server = server.unwrap();
                        assert_live(client, false);
                        assert_live(server, true);
                        assert!(client.observed_at >= client_live.observed_at);
                        assert!(server.observed_at >= server_live.observed_at);
                    }
                    {
                        let mut cancelled = std::pin::pin!(client_observation.snapshot());
                        assert!(futures::poll!(cancelled.as_mut()).is_pending());
                    }
                    assert!(
                        client_observation
                            .snapshot()
                            .await
                            .unwrap()
                            .exchange
                            .is_some()
                    );
                    allow.send(()).unwrap();
                    (client_live, server_live)
                };
                let (response, live) = futures::join!(client.send(request), inspect);
                let mut response = response.unwrap();
                assert_eq!(response.status(), 200);
                assert!(
                    client_observation
                        .snapshot()
                        .await
                        .unwrap()
                        .exchange
                        .is_some()
                );
                assert_eq!(response.body_mut().collect(64).await.unwrap(), b"answer");
                client.shutdown().await.unwrap();
                live
            };
            let ((client_live, server_live), client, server) =
                futures::join!(app, client_driver, server);
            client.unwrap();
            server.unwrap();
            assert_eq!(handled.get(), 1);
            for (observation, old, issued) in [
                (&client_observation, client_live, client_issued),
                (&server_observation, server_live, server_issued),
            ] {
                let snapshot = observation.final_snapshot().unwrap();
                assert_eq!(snapshot.phase, SnapshotPhase::Closed);
                assert!(snapshot.exchange.is_none());
                assert!(!snapshot.read_outstanding);
                assert!(!snapshot.write_outstanding);
                assert!(!snapshot.body_lease_outstanding);
                assert_eq!(snapshot.failure, None);
                assert_eq!(snapshot.connection, old.connection);
                assert!(snapshot.observed_at >= old.observed_at);
                assert_live(old, snapshot.server);
                let counters = snapshot.counters;
                assert_eq!(
                    counters.read_bytes,
                    if snapshot.server {
                        REQUEST.len()
                    } else {
                        RESPONSE.len()
                    } as u64
                );
                assert_eq!(
                    counters.written_bytes_lower_bound,
                    if snapshot.server {
                        RESPONSE.len()
                    } else {
                        REQUEST.len()
                    } as u64
                );
                assert_eq!(
                    counters.body_bytes_delivered,
                    if snapshot.server { 0 } else { 6 }
                );
                assert_eq!(
                    counters.body_bytes_consumed,
                    if snapshot.server { 0 } else { 6 }
                );
                assert_eq!(
                    counters.producer_bytes_accepted,
                    if snapshot.server { 6 } else { 0 }
                );
                assert_eq!(counters.exchanges_started, 1);
                assert_eq!(counters.exchanges_retired, 1);
                assert_eq!(counters.exchanges_failed, 0);
                assert_eq!(counters.cancellation_requests, 0);
                assert_eq!(counters.deadline_expirations, 0);
                assert_eq!(counters.uncertain_write_completions, 0);
                assert!(!counters.saturated);
                // Read fragmentation can vary. Every issued operation still has one settled completion.
                assert_eq!(
                    (
                        counters.read_completions,
                        counters.write_completions,
                        counters.body_deliveries
                    ),
                    issued.get(),
                );
                assert_eq!(observation.snapshot().await.unwrap(), snapshot);
                assert_eq!(observation.snapshot().await.unwrap(), snapshot);
                assert_eq!(observation.final_snapshot(), Some(snapshot));
            }
        }
    }

    #[kimojio::test]
    async fn startup_failure_drains_queued_and_blocked_snapshot_requests() {
        for native in [false, true] {
            for server in [false, true] {
                let observation = Observation::new();
                let mut config = config(20, &observation);
                config.protocol.max_headers = 0;
                let (fd, peer) = kimojio::pipe::bipipe();
                let (_client, driver) = driver(native, server, fd, config);
                let mut queued = std::pin::pin!(observation.snapshot());
                let mut blocked = std::pin::pin!(observation.snapshot());
                assert!(futures::poll!(queued.as_mut()).is_pending());
                assert!(futures::poll!(blocked.as_mut()).is_pending());
                assert!(matches!(
                    driver.await,
                    Err(Error::Command(
                        kimojio_fsm_http1::CommandError::InvalidConfig
                    ))
                ));
                assert!(matches!(
                    futures::poll!(queued),
                    Poll::Ready(Err(Error::Closed))
                ));
                assert!(matches!(
                    futures::poll!(blocked),
                    Poll::Ready(Err(Error::Closed))
                ));
                assert!(observation.final_snapshot().is_none());
                assert!(matches!(observation.snapshot().await, Err(Error::Closed)));
                assert_eq!(operations::read(&peer, &mut [0; 1]).await.unwrap(), 0);
            }
        }
    }

    #[kimojio::test]
    async fn abort_caches_the_terminal_failure_after_actual_transport_close() {
        use kimojio_http1::{
            Shutdown, serve_connection_native_with_shutdown, serve_connection_with_shutdown,
        };
        for native in [false, true] {
            let observation = Observation::new();
            let (fd, peer) = kimojio::pipe::bipipe();
            let shutdown = Shutdown::default();
            let handler = |_| async { panic!("no request was sent") };
            let mut server = if native {
                serve_connection_native_with_shutdown(
                    fd,
                    config(24, &observation),
                    shutdown.clone(),
                    handler,
                )
                .boxed_local()
            } else {
                serve_connection_with_shutdown(
                    OwnedFdStream::new(fd),
                    config(24, &observation),
                    shutdown.clone(),
                    handler,
                )
                .boxed_local()
            };
            assert!(futures::poll!(server.as_mut()).is_pending());
            assert!(observation.final_snapshot().is_none());
            shutdown.abort();
            assert!(matches!(
                server.await,
                Err(Error::Protocol(kimojio_fsm_http1::Failure::Cancelled))
            ));
            assert_eq!(operations::read(&peer, &mut [0; 1]).await.unwrap(), 0);
            let final_snapshot = observation.final_snapshot().unwrap();
            assert_eq!(final_snapshot.phase, SnapshotPhase::Closed);
            assert_eq!(
                final_snapshot.failure,
                Some(kimojio_fsm_http1::Failure::Cancelled)
            );
            assert!(!final_snapshot.read_outstanding);
            assert!(!final_snapshot.write_outstanding);
            let mut expected = Counters::default();
            expected.read_completions = 1;
            expected.cancellation_requests = 1;
            assert_eq!(final_snapshot.counters, expected);
            assert_eq!(observation.snapshot().await.unwrap(), final_snapshot);
        }
    }

    #[kimojio::test]
    async fn unpolled_driver_drop_closes_pending_queries_without_a_final_snapshot() {
        for native in [false, true] {
            for server in [false, true] {
                let observation = Observation::new();
                let (fd, _peer) = kimojio::pipe::bipipe();
                let (_client, driver) = driver(native, server, fd, config(21, &observation));
                let mut queued = std::pin::pin!(observation.snapshot());
                let mut blocked = std::pin::pin!(observation.snapshot());
                assert!(futures::poll!(queued.as_mut()).is_pending());
                assert!(futures::poll!(blocked.as_mut()).is_pending());
                drop(driver);
                assert!(matches!(
                    futures::poll!(queued),
                    Poll::Ready(Err(Error::Closed))
                ));
                assert!(matches!(
                    futures::poll!(blocked),
                    Poll::Ready(Err(Error::Closed))
                ));
                assert!(observation.final_snapshot().is_none());
                assert!(matches!(observation.snapshot().await, Err(Error::Closed)));
            }
        }
    }

    struct FailingSplit;

    impl SplittableStream for FailingSplit {
        type ReadStream = kimojio::OwnedFdStreamRead;
        type WriteStream = kimojio::OwnedFdStreamWrite;

        async fn split(self) -> Result<(Self::ReadStream, Self::WriteStream), kimojio::Errno> {
            Err(kimojio::Errno::IO)
        }
    }

    #[kimojio::test]
    async fn transport_startup_failure_closes_snapshot_promises() {
        let observation = Observation::new();
        let (_client, driver) = connect(FailingSplit, config(22, &observation));
        let mut query = std::pin::pin!(observation.snapshot());
        assert!(futures::poll!(query.as_mut()).is_pending());
        assert!(matches!(driver.run().await, Err(Error::Transport(_))));
        assert!(matches!(
            futures::poll!(query),
            Poll::Ready(Err(Error::Closed))
        ));
        assert!(observation.final_snapshot().is_none());
    }

    struct PendingSplit;

    impl SplittableStream for PendingSplit {
        type ReadStream = kimojio::OwnedFdStreamRead;
        type WriteStream = kimojio::OwnedFdStreamWrite;

        async fn split(self) -> Result<(Self::ReadStream, Self::WriteStream), kimojio::Errno> {
            std::future::pending().await
        }
    }

    #[kimojio::test]
    async fn cancelling_started_driver_during_split_closes_pending_queries() {
        let observation = Observation::new();
        let (_client, driver) = connect(PendingSplit, config(23, &observation));
        let mut driver = Box::pin(driver.run());
        assert!(futures::poll!(driver.as_mut()).is_pending());
        let mut query = std::pin::pin!(observation.snapshot());
        assert!(futures::poll!(query.as_mut()).is_pending());
        drop(driver);
        assert!(matches!(
            futures::poll!(query),
            Poll::Ready(Err(Error::Closed))
        ));
        assert!(observation.final_snapshot().is_none());
    }
}
