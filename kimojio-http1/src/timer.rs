//! One logical deadline and at most one physical wake per connection.
//!
//! A wake earlier than the logical deadline is harmless: recheck and arm the
//! current deadline when it fires. Replacing it on every progress notification
//! would cancel and recreate kernel timers unnecessarily. Never retain a wake
//! later than the logical deadline, and never expose an early wake as expiry.

use kimojio::{Errno, operations};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

#[derive(Default)]
pub(crate) struct DeadlineTimer {
    desired: Option<Instant>,
    armed: Option<(Instant, operations::SleepFuture<'static>)>,
}

impl DeadlineTimer {
    pub(crate) fn set(&mut self, desired: Option<Instant>) {
        self.desired = desired;
        if desired.is_none()
            || self
                .armed
                .as_ref()
                .is_some_and(|(at, _)| Some(*at) > desired)
        {
            self.armed = None;
        }
        // Allocate only when the driver actually polls the timer lane.
    }

    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Errno>> {
        let Some(desired) = self.desired else {
            return Poll::Pending;
        };
        loop {
            let (at, timer) = self
                .armed
                .get_or_insert_with(|| (desired, operations::sleep_until(desired)));
            let at = *at;
            let result = std::task::ready!(Pin::new(timer).poll(cx));
            self.armed = None;
            match result {
                Ok(()) if at < desired && kimojio::clock_now() < desired => continue,
                result => return Poll::Ready(result),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[kimojio::test]
    async fn dormant_timer_does_not_create_a_native_future_and_can_move() {
        let mut timer = DeadlineTimer::default();
        timer.set(Some(kimojio::clock_now() + Duration::from_secs(60)));
        assert!(timer.armed.is_none());
        assert!(
            futures::future::poll_fn(|cx| Poll::Ready(timer.poll(cx)))
                .await
                .is_pending()
        );
        let mut moved = std::hint::black_box(timer);
        moved.set(Some(kimojio::clock_now()));
        futures::future::poll_fn(|cx| moved.poll(cx)).await.unwrap();
        assert!(moved.armed.is_none());
        moved.set(None);
        assert!(
            futures::future::poll_fn(|cx| Poll::Ready(moved.poll(cx)))
                .await
                .is_pending()
        );
    }

    #[kimojio::test]
    async fn scope_cancellation_of_an_earlier_wake_is_not_hidden() {
        let mut timer = DeadlineTimer::default();
        kimojio::operations::io_scope(async || {
            let now = kimojio::clock_now();
            timer.set(Some(now + Duration::from_secs(60)));
            assert!(
                futures::future::poll_fn(|cx| Poll::Ready(timer.poll(cx)))
                    .await
                    .is_pending()
            );
            timer.set(Some(now + Duration::from_secs(120)));
            operations::io_scope_cancel();
            assert_eq!(
                futures::future::poll_fn(|cx| timer.poll(cx)).await,
                Err(Errno::CANCELED)
            );
            timer.set(None);
        })
        .await;
    }

    #[cfg(feature = "virtual-clock")]
    #[kimojio::test]
    async fn postponed_deadlines_reuse_one_wake_but_earlier_deadlines_replace_it() {
        operations::virtual_clock_enable(true);
        let epoch = kimojio::clock_now();
        let mut timer = DeadlineTimer::default();
        let poll = |timer: &mut DeadlineTimer| {
            timer.poll(&mut Context::from_waker(std::task::Waker::noop()))
        };
        timer.set(Some(epoch + Duration::from_secs(10)));
        assert!(poll(&mut timer).is_pending());
        for n in 11..=100 {
            timer.set(Some(epoch + Duration::from_secs(n)));
            assert!(poll(&mut timer).is_pending());
            assert_eq!(
                operations::virtual_clock_next_deadline(),
                Some(epoch + Duration::from_secs(10))
            );
            assert_eq!(operations::virtual_clock_pending_timers(), 1);
        }
        operations::virtual_clock_advance(Duration::from_secs(10));
        assert!(
            poll(&mut timer).is_pending(),
            "virtual sleeps yield once when becoming ready"
        );
        assert!(
            poll(&mut timer).is_pending(),
            "obsolete early wake is not expiration"
        );
        assert_eq!(
            operations::virtual_clock_next_deadline(),
            Some(epoch + Duration::from_secs(100))
        );
        timer.set(Some(epoch + Duration::from_secs(20)));
        assert!(poll(&mut timer).is_pending());
        assert_eq!(operations::virtual_clock_pending_timers(), 1);
        assert_eq!(
            operations::virtual_clock_next_deadline(),
            Some(epoch + Duration::from_secs(20))
        );
        operations::virtual_clock_advance(Duration::from_secs(10));
        assert!(poll(&mut timer).is_pending());
        assert_eq!(poll(&mut timer), Poll::Ready(Ok(())));
        timer.set(None);
        assert!(poll(&mut timer).is_pending());
        assert_eq!(operations::virtual_clock_pending_timers(), 0);
    }

    #[cfg(feature = "virtual-clock")]
    #[kimojio::test]
    async fn delayed_early_wake_reports_expiry_without_a_second_kernel_wake() {
        operations::virtual_clock_enable(true);
        let now = kimojio::clock_now();
        let mut timer = DeadlineTimer::default();
        let poll = |timer: &mut DeadlineTimer| {
            timer.poll(&mut Context::from_waker(std::task::Waker::noop()))
        };
        timer.set(Some(now + Duration::from_secs(1)));
        assert!(poll(&mut timer).is_pending());
        timer.set(Some(now + Duration::from_secs(2)));
        operations::virtual_clock_advance(Duration::from_secs(3));
        assert!(poll(&mut timer).is_pending()); // virtual sleep's cooperative yield
        assert_eq!(poll(&mut timer), Poll::Ready(Ok(())));
        assert_eq!(operations::virtual_clock_pending_timers(), 0);
    }

    #[cfg(feature = "virtual-clock")]
    #[kimojio::test]
    async fn clearing_or_replacing_a_fired_wake_never_expires_a_new_deadline() {
        operations::virtual_clock_enable(true);
        let now = kimojio::clock_now();
        let mut timer = DeadlineTimer::default();
        let poll = |timer: &mut DeadlineTimer| {
            timer.poll(&mut Context::from_waker(std::task::Waker::noop()))
        };
        timer.set(Some(now + Duration::from_secs(1)));
        assert!(poll(&mut timer).is_pending());
        operations::virtual_clock_advance(Duration::from_secs(2));
        timer.set(None);
        timer.set(Some(now + Duration::from_secs(3)));
        assert!(poll(&mut timer).is_pending());
        operations::virtual_clock_advance(Duration::from_secs(1));
        assert!(poll(&mut timer).is_pending());
        assert_eq!(poll(&mut timer), Poll::Ready(Ok(())));
    }
}
