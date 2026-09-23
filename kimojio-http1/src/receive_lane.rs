//! Keep a receive future (and its runtime wait registration) across
//! unrelated driver inputs. One boxed stream per lane, not per message.
use futures::{StreamExt, stream::LocalBoxStream};
use kimojio::{CancellationToken, ChannelError, Receiver};
use std::{
    rc::Rc,
    task::{Context, Poll},
};

pub(crate) struct CancelLane {
    token: Rc<CancellationToken>,
    wait: Option<futures::future::LocalBoxFuture<'static, Result<(), kimojio::CanceledError>>>,
}
impl CancelLane {
    pub(crate) fn new(token: Rc<CancellationToken>) -> Self {
        Self { token, wait: None }
    }
    pub(crate) fn clear(&mut self) {
        self.wait = None;
    }
    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let wait = self.wait.get_or_insert_with(|| {
            let token = self.token.clone();
            Box::pin(async move { token.cancelled().await })
        });
        let result = wait.as_mut().poll(cx);
        if result.is_ready() {
            self.wait = None;
        }
        result.map(|_| ())
    }
}

pub(crate) struct ReceiveLane<T> {
    receiver: Rc<Receiver<T>>,
    stream: LocalBoxStream<'static, Result<T, ChannelError>>,
    waiting: bool,
}
impl<T: 'static> ReceiveLane<T> {
    pub(crate) fn new(receiver: Receiver<T>) -> Self {
        let receiver = Rc::new(receiver);
        let stream = futures::stream::unfold(receiver.clone(), |receiver| async {
            let value = receiver.recv().await;
            Some((value, receiver))
        })
        .boxed_local();
        Self {
            receiver,
            stream,
            waiting: false,
        }
    }
    pub(crate) fn try_recv(&self) -> Result<Option<T>, ChannelError> {
        self.receiver.try_recv()
    }
    pub(crate) fn poll(
        &mut self,
        cx: &mut Context<'_>,
        runnable: bool,
    ) -> Poll<Result<T, ChannelError>> {
        if runnable && !self.waiting {
            return match self.receiver.try_recv() {
                Ok(Some(item)) => Poll::Ready(Ok(item)),
                Ok(None) => Poll::Pending,
                Err(error) => Poll::Ready(Err(error)),
            };
        }
        let result = self
            .stream
            .as_mut()
            .poll_next(cx)
            .map(|item| item.expect("unending receive lane"));
        self.waiting = result.is_pending();
        if matches!(result, Poll::Ready(Err(ChannelError::Canceled))) {
            // The selector ignores canceled release/demand waits. Schedule a
            // fresh pass so a queued value or replacement wait is not stranded
            // after the old registration was consumed by cancellation.
            cx.waker().wake_by_ref();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runnable_empty_lane_does_not_register_a_runtime_wait() {
        let (send, receive) = kimojio::async_channel();
        let mut lane = ReceiveLane::new(receive);
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(lane.poll(&mut cx, true).is_pending());
        send.try_send(7).unwrap();
        assert_eq!(lane.poll(&mut cx, true), Poll::Ready(Ok(7)));
    }
    #[kimojio::test]
    async fn pending_lane_survives_other_inputs_wakes_and_channel_close() {
        let (send, receive) = kimojio::async_channel();
        let mut lane = ReceiveLane::new(receive);
        for value in 0..32 {
            for _ in 0..3 {
                assert!(
                    futures::future::poll_fn(|cx| Poll::Ready(lane.poll(cx, false)))
                        .await
                        .is_pending()
                );
            }
            send.try_send(value).unwrap();
            assert_eq!(
                futures::future::poll_fn(|cx| lane.poll(cx, true)).await,
                Ok(value)
            );
        }
        drop(send);
        assert_eq!(
            futures::future::poll_fn(|cx| lane.poll(cx, false)).await,
            Err(ChannelError::Closed)
        );
    }
    #[kimojio::test]
    async fn cancellation_schedules_a_fresh_pass_even_if_a_message_is_already_queued() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Count(AtomicUsize);
        impl std::task::Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = std::task::Waker::from(count.clone());
        let mut cx = Context::from_waker(&waker);
        let (send, receiver) = kimojio::async_channel();
        let mut lane = ReceiveLane::new(receiver);
        kimojio::operations::io_scope(async || {
            assert!(lane.poll(&mut cx, false).is_pending());
            kimojio::operations::io_scope_cancel();
            send.try_send(9).unwrap();
            let before = count.0.load(Ordering::Relaxed);
            assert_eq!(
                lane.poll(&mut cx, true),
                Poll::Ready(Err(ChannelError::Canceled))
            );
            assert!(count.0.load(Ordering::Relaxed) > before);
        })
        .await;
        assert_eq!(lane.poll(&mut cx, true), Poll::Ready(Ok(9)));
    }

    #[kimojio::test]
    async fn cancel_lane_retires_completed_and_canceled_waits() {
        let token = Rc::new(CancellationToken::new());
        let mut lane = CancelLane::new(token.clone());
        kimojio::operations::io_scope(async || {
            assert!(
                futures::future::poll_fn(|cx| Poll::Ready(lane.poll(cx)))
                    .await
                    .is_pending()
            );
            kimojio::operations::io_scope_cancel();
            futures::future::poll_fn(|cx| lane.poll(cx)).await;
            assert!(!token.is_cancelled());
        })
        .await;
        assert!(
            futures::future::poll_fn(|cx| Poll::Ready(lane.poll(cx)))
                .await
                .is_pending()
        );
        token.cancel();
        futures::future::poll_fn(|cx| lane.poll(cx)).await;
        lane.clear();
        assert_eq!(
            Rc::strong_count(&token),
            2,
            "no wait may retain a retired token owner"
        );
    }

    #[kimojio::test]
    async fn canceled_scope_is_observed_then_a_new_receive_has_no_old_membership() {
        let (send, receive) = kimojio::async_channel();
        let mut lane = ReceiveLane::new(receive);
        kimojio::operations::io_scope(async || {
            assert!(
                futures::future::poll_fn(|cx| Poll::Ready(lane.poll(cx, false)))
                    .await
                    .is_pending()
            );
            kimojio::operations::io_scope_cancel();
            assert!(
                futures::future::poll_fn(|cx| lane.poll(cx, false))
                    .await
                    .is_err()
            );
        })
        .await;
        send.try_send(7).unwrap();
        assert_eq!(
            futures::future::poll_fn(|cx| lane.poll(cx, false)).await,
            Ok(7)
        );
    }
}
