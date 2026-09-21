use super::*;
use crate::Observation;

#[cfg(feature = "diagnostics")]
#[test]
fn diagnostic_port_forwards_immediately_without_holding_a_handle_borrow() {
    use std::cell::RefCell;

    let calls = Rc::new(Cell::new(0));
    let holder = Rc::new(RefCell::new(None::<Observation>));
    let callback_holder = holder.clone();
    let callback_calls = calls.clone();
    let id = core::ConnectionId {
        slot: 90,
        generation: 2,
    };
    let event = core::LogEvent::PrimaryFailure(core::Failure::Timeout);
    let observation = Observation::with_logger(move |connection, now, received| {
        assert_eq!(connection, id);
        assert_eq!(now, core::Tick(71));
        assert_eq!(received, event);
        let observation = callback_holder.borrow_mut().take().unwrap();
        assert!(matches!(observation.bind(), Err(Error::ObservationInUse)));
        #[cfg(feature = "metrics")]
        assert!(observation.final_snapshot().is_none());
        *callback_holder.borrow_mut() = Some(observation);
        callback_calls.set(callback_calls.get() + 1);
    });
    *holder.borrow_mut() = Some(observation.clone());
    let _bound = observation.bind().unwrap();
    let mut ports = Ports {
        observation: Some(observation),
    };
    for expected in 1..=8 {
        core::Ports::log(&mut ports, id, core::Tick(71), event);
        assert_eq!(calls.get(), expected);
    }
    assert!(matches!(
        core::Ports::closed(&mut ports, Ok(())),
        Some(Event::Closed(Ok(())))
    ));
    assert_eq!(calls.get(), 8);
    core::Ports::log(&mut Ports::default(), id, core::Tick(71), event);
    assert_eq!(calls.get(), 8);
    holder.borrow_mut().take();
}

#[cfg(feature = "metrics")]
fn observed_state() -> (Observation, State) {
    let observation = Observation::new();
    let mut config = Config::new(core::ConnectionId {
        slot: 91,
        generation: 1,
    });
    config.observation = Some(observation.clone());
    config.protocol.idle_timeout_ns = None;
    let state = State::new(config, false, Shutdown::default()).unwrap();
    (observation, state)
}

#[cfg(feature = "metrics")]
#[kimojio::test]
async fn snapshots_and_requests_share_round_robin_input_admission() {
    let (observation, mut state) = observed_state();
    let (fd, _peer) = kimojio::pipe::bipipe();
    let mut io = native_io(fd);
    let (send, requests) = async_channel();
    let (response, _receive) = oneshot();
    send.try_send(SendRequest {
        request: Request::new(OutgoingBody::empty()),
        response,
        cancel: Rc::new(CancellationToken::new()),
    })
    .unwrap_or_else(|_| panic!("empty request channel"));
    let mut snapshot = std::pin::pin!(observation.snapshot());
    assert!(futures::poll!(snapshot.as_mut()).is_pending());
    state.rotation = 9;
    let input = next_input(&mut state, &mut io, &requests, true)
        .await
        .unwrap();
    assert!(matches!(input, Input::Snapshot(_)));
    state.input(input, &mut io).unwrap();
    assert_eq!(snapshot.await.unwrap(), state.metrics());
    let mut snapshot = std::pin::pin!(observation.snapshot());
    assert!(futures::poll!(snapshot.as_mut()).is_pending());
    let input = next_input(&mut state, &mut io, &requests, true)
        .await
        .unwrap();
    assert!(matches!(input, Input::Request(Ok(_))));
    drop(input);
    let input = next_input(&mut state, &mut io, &requests, true)
        .await
        .unwrap();
    assert!(matches!(input, Input::Snapshot(_)));
    state.input(input, &mut io).unwrap();
    assert_eq!(snapshot.await.unwrap(), state.metrics());
}

#[cfg(feature = "metrics")]
#[kimojio::test]
async fn final_cache_waits_for_closed_and_guard_drop_answers_queued_and_blocked_queries() {
    let (observation, mut state) = observed_state();
    let (fd, _peer) = kimojio::pipe::bipipe();
    let mut io = native_io(fd);
    let mut queued = std::pin::pin!(observation.snapshot());
    let mut blocked = std::pin::pin!(observation.snapshot());
    assert!(futures::poll!(queued.as_mut()).is_pending());
    assert!(futures::poll!(blocked.as_mut()).is_pending());
    let Machine::Client(client) = &mut state.machine else {
        unreachable!()
    };
    client.shutdown(core::ShutdownMode::Abort);
    let closed = loop {
        assert!(observation.final_snapshot().is_none());
        let event = client
            .next(&mut Ports::default())
            .expect("close must make progress");
        match event {
            Event::Deadline(_) => {}
            Event::Close(op) => {
                client.complete_close(op.complete(Ok(()))).unwrap();
                assert!(observation.final_snapshot().is_none());
            }
            event @ Event::Closed(Err(core::Failure::Cancelled)) => break event,
            _ => panic!("unexpected close event"),
        }
    };
    let result = state
        .event(
            closed,
            &mut |_| std::future::ready(Ok(Response::new(OutgoingBody::empty()))),
            &mut io,
        )
        .unwrap();
    assert!(matches!(
        result,
        Some(Err(Error::Protocol(core::Failure::Cancelled)))
    ));
    let expected = state.metrics();
    assert_eq!(expected.phase, core::SnapshotPhase::Closed);
    assert_eq!(observation.final_snapshot(), Some(expected));
    assert_eq!(observation.snapshot().await.unwrap(), expected);
    drop(state);
    assert_eq!(queued.await.unwrap(), expected);
    assert_eq!(blocked.await.unwrap(), expected);
}
