use super::*;

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

#[kimojio::test]
async fn already_due_initial_deadline_is_applied_before_issuing_io() {
    let mut config = Config::new(core::ConnectionId {
        slot: 77,
        generation: 1,
    });
    config.protocol.head_timeout_ns = Some(0);
    let state = State::new(config, true, Shutdown::default()).unwrap();
    let (_send, requests) = async_channel();
    let mut handler = |_| std::future::ready(Ok(Response::new(OutgoingBody::empty())));
    let result = drive(state, NoReads { close: None }, &requests, &mut handler, 64).await;
    assert!(matches!(
        result,
        Err(Error::Protocol(core::Failure::Timeout))
    ));
}
