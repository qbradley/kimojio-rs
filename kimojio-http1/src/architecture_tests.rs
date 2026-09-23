use super::*;

#[test]
fn deadline_hints_coalesce_only_inside_the_remaining_turn_budget() {
    use kimojio_fsm_http1::Ports as _;
    for budget in [0, 1, 2, 8, 64] {
        let mut ports = Ports {
            deadline_budget: budget,
            ..Ports::default()
        };
        for count in 0..budget.saturating_sub(1) {
            assert!(ports.deadline_changed(None).is_none());
            assert_eq!(ports.deferred_count, count + 1);
            assert_eq!(ports.deferred_deadline, Some(None));
        }
        assert!(matches!(
            ports.deadline_changed(None),
            Some(Event::Deadline(None))
        ));
    }
}

#[kimojio::test]
async fn disabling_policy_timers_does_not_disable_observation_timestamps() {
    for observing in [false, true] {
        let seen = Rc::new(Cell::new(core::Tick(0)));
        let mut config = Config::new(core::ConnectionId {
            slot: 79,
            generation: 1,
        });
        config.protocol.head_timeout_ns = None;
        config.protocol.body_timeout_ns = None;
        config.protocol.idle_timeout_ns = None;
        config.protocol.continue_timeout_ns = None;
        if observing {
            let seen = seen.clone();
            config.observation = Some(crate::Observation::with_logger(move |_, now, _| {
                seen.set(now)
            }));
        }
        let mut state = State::new(config, true, Shutdown::default()).unwrap();
        state.epoch = kimojio::clock_now() - Duration::from_secs(1);
        let now = state.observe().unwrap();
        assert!(!state.uses_deadlines);
        if observing {
            assert!(now.0 >= 1_000_000_000);
        } else {
            assert_eq!(now, core::Tick(0));
        }
        let mut ports = Ports {
            observation: state
                .observation
                .as_ref()
                .map(|bound| bound.observation.clone()),
            ..Ports::default()
        };
        let Machine::Server(server) = &mut state.machine else {
            unreachable!()
        };
        let Some(Event::Read(op)) = server.next(&mut ports) else {
            panic!()
        };
        if observing {
            assert_eq!(seen.get(), now);
        }
        server.complete_read(op.complete(Ok(0))).unwrap();
    }
}

struct NoReads {
    close: Option<core::CloseCompletion>,
}
impl IoDriver for NoReads {
    fn read(&mut self, _: core::ReadOp<Vec<u8>>) -> Result<(), Error> {
        panic!("expired deadline issued a read")
    }
    fn write(&mut self, _: core::WriteOp<OutgoingData>) -> Result<(), Error> {
        panic!("idle timeout issued a write")
    }
    fn cancel_read(&self) {}
    fn cancel_write(&self) {}
    fn close(&mut self, op: core::CloseOp) -> Result<(), Error> {
        self.close = Some(op.complete(Ok(())));
        Ok(())
    }
    fn completions(
        &mut self,
        _: bool,
    ) -> (
        impl Future<Output = core::ReadCompletion<Vec<u8>>> + '_,
        impl Future<Output = WriteResult> + '_,
    ) {
        let close = self.close.take();
        (std::future::pending(), async move {
            match close {
                Some(close) => WriteResult::Close(close),
                None => std::future::pending().await,
            }
        })
    }
}

struct ErrorResponseIo {
    result: Option<WriteResult>,
}
impl IoDriver for ErrorResponseIo {
    fn read(&mut self, _: core::ReadOp<Vec<u8>>) -> Result<(), Error> {
        panic!("expired pipelined head issued read")
    }
    fn write(&mut self, op: core::WriteOp<OutgoingData>) -> Result<(), Error> {
        assert!(op.slices().concat().starts_with(b"HTTP/1.1 408 "));
        let count = op.slices().iter().map(|s| s.len()).sum();
        assert!(
            self.result
                .replace(WriteResult::Write(op.complete(Ok(count))))
                .is_none()
        );
        Ok(())
    }
    fn cancel_read(&self) {}
    fn cancel_write(&self) {}
    fn close(&mut self, op: core::CloseOp) -> Result<(), Error> {
        assert!(
            self.result
                .replace(WriteResult::Close(op.complete(Ok(()))))
                .is_none()
        );
        Ok(())
    }
    fn completions(
        &mut self,
        _: bool,
    ) -> (
        impl Future<Output = core::ReadCompletion<Vec<u8>>> + '_,
        impl Future<Output = WriteResult> + '_,
    ) {
        let result = self.result.take();
        (std::future::pending(), async move {
            match result {
                Some(result) => result,
                None => std::future::pending().await,
            }
        })
    }
}

#[kimojio::test]
async fn deadline_created_inside_clocked_drive_preempts_pipelined_dispatch() {
    let mut config = Config::new(core::ConnectionId {
        slot: 78,
        generation: 1,
    });
    config.protocol.head_timeout_ns = Some(0);
    config.protocol.body_timeout_ns = None;
    config.protocol.idle_timeout_ns = None;
    let mut state = State::new(config, true, Shutdown::default()).unwrap();
    let Machine::Server(server) = &mut state.machine else {
        unreachable!()
    };
    // Seed an already-completed exchange with the legacy manual clock API.
    // The second head is buffered, but its head timer does not exist yet.
    let read = loop {
        match server.next(&mut Ports::default()).unwrap() {
            Event::Deadline(_) => {}
            Event::Read(read) => break read,
            _ => panic!(),
        }
    };
    let mut read = read;
    let bytes = b"GET / HTTP/1.1\r\nhost: a\r\n\r\nGET /next HTTP/1.1\r\nhost: a\r\n\r\n";
    read.bytes_mut()[..bytes.len()].copy_from_slice(bytes);
    server
        .complete_read(read.complete(Ok(bytes.len())))
        .unwrap();
    let exchange = loop {
        match server.next(&mut Ports::default()).unwrap() {
            Event::Deadline(_) => {}
            Event::Request(id, _) => break id,
            _ => panic!(),
        }
    };
    server
        .respond(
            exchange,
            core::Response::new(200, "OK", &[], core::BodyLength::Empty),
        )
        .unwrap();
    loop {
        match server.next(&mut Ports::default()).unwrap() {
            Event::Write(op) => {
                let len = op.slices().iter().map(|s| s.len()).sum();
                server.complete_write(op.complete(Ok(len))).unwrap();
            }
            Event::Deadline(_) | Event::SourceFinished(_) | Event::IncomingFinished(_) => {}
            Event::ExchangeFinished(f) => {
                assert!(f.reusable);
                break;
            }
            _ => panic!(),
        }
    }
    let (_send, requests) = async_channel();
    let mut handler = |_| -> std::future::Ready<Result<Response<OutgoingBody>, Error>> {
        panic!("expired reused head reached handler")
    };
    let result = drive(
        state,
        ErrorResponseIo { result: None },
        &requests,
        &mut handler,
        64,
    )
    .await;
    assert!(matches!(
        result,
        Err(Error::Protocol(core::Failure::Timeout))
    ));
}

#[kimojio::test]
async fn unrepresentable_wake_epoch_keeps_the_yielding_error_path() {
    let mut config = Config::new(core::ConnectionId {
        slot: 80,
        generation: 1,
    });
    config.protocol.head_timeout_ns = Some(u64::MAX);
    let mut state = State::new(config, true, Shutdown::default()).unwrap();
    // Find a valid but extreme Instant with no room for a u64 nanosecond Tick.
    for bit in (0..64).rev() {
        if let Some(later) = state.epoch.checked_add(Duration::from_secs(1u64 << bit)) {
            state.epoch = later;
        }
    }
    assert!(
        state
            .epoch
            .checked_add(Duration::from_nanos(u64::MAX))
            .is_none()
    );
    let (_send, requests) = async_channel();
    let mut handler = |_| std::future::ready(Ok(Response::new(OutgoingBody::empty())));
    let result = drive(state, NoReads { close: None }, &requests, &mut handler, 64).await;
    assert!(
        result.is_err(),
        "conversion failure must precede I/O issuance"
    );
}

#[kimojio::test]
async fn already_due_initial_deadline_is_applied_before_issuing_io() {
    for abort_requested in [false, true] {
        let mut config = Config::new(core::ConnectionId {
            slot: 77,
            generation: 1,
        });
        config.protocol.head_timeout_ns = Some(0);
        let shutdown = Shutdown::default();
        if abort_requested {
            shutdown.abort();
        }
        let state = State::new(config, true, shutdown).unwrap();
        let (_send, requests) = async_channel();
        let mut handler = |_| std::future::ready(Ok(Response::new(OutgoingBody::empty())));
        let result = drive(state, NoReads { close: None }, &requests, &mut handler, 64).await;
        assert!(
            matches!(result, Err(Error::Protocol(core::Failure::Timeout))),
            "expiry must retain priority over a concurrent abort request"
        );
    }
}
