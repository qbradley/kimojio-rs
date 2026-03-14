// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Clock abstraction for deterministic timing in tests.
//!
//! This module provides a `clock_now()` helper used internally by deadline-based
//! operations to obtain the current time. When the `virtual-clock` feature is enabled,
//! this returns the virtual clock's time if one is installed in the runtime; otherwise
//! it returns [`std::time::Instant::now()`].
//!
//! The `virtual-clock` feature additionally provides the [`Clock`] trait,
//! [`SystemClock`], and [`VirtualClock`] types for deterministic test timing.

use std::time::Instant;

/// Returns the current instant from the runtime's clock.
///
/// When the `virtual-clock` feature is enabled and a virtual clock is installed
/// in the current runtime, returns the virtual clock's time. Otherwise returns
/// the real system time via [`Instant::now()`].
#[cfg(feature = "virtual-clock")]
#[allow(dead_code)] // Used in Phase 2 & 3: operations.rs, async_event.rs, async_lock.rs
pub(crate) fn clock_now() -> Instant {
    let task_state = crate::task::TaskState::get();
    match &task_state.clock {
        Some(clock) => clock.now(),
        None => Instant::now(),
    }
}

/// Returns the current instant from the system clock.
///
/// When the `virtual-clock` feature is not enabled, this always returns
/// [`Instant::now()`].
#[cfg(not(feature = "virtual-clock"))]
#[allow(dead_code)] // Used in Phase 2 & 3: operations.rs, async_event.rs, async_lock.rs
pub(crate) fn clock_now() -> Instant {
    Instant::now()
}

// --- Virtual clock types (feature-gated) ---

#[cfg(feature = "virtual-clock")]
mod virtual_clock {
    use std::cell::RefCell;
    use std::cmp::Ordering;
    use std::collections::BinaryHeap;
    use std::rc::Rc;
    use std::task::Waker;
    use std::time::{Duration, Instant};

    /// Clock provider for runtime timing operations.
    ///
    /// Production code uses [`SystemClock`] (the default), which delegates to real
    /// system time and io_uring timers. Test code can inject a [`VirtualClock`] to
    /// get deterministic, instant timer resolution via manual time advancement.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use kimojio::clock::{Clock, SystemClock};
    ///
    /// let clock = SystemClock;
    /// let now = clock.now();
    /// assert!(clock.is_real());
    /// ```
    pub trait Clock: 'static {
        /// Returns the current instant according to this clock.
        fn now(&self) -> Instant;

        /// Registers a timer to fire at `deadline`, returning a unique [`TimerId`].
        ///
        /// When the deadline is reached (via time advancement for virtual clocks),
        /// the provided [`Waker`] will be called to wake the associated task.
        fn register_timer(&self, deadline: Instant, waker: Waker) -> TimerId;

        /// Cancels a previously registered timer.
        ///
        /// If the timer has already fired or does not exist, this is a no-op.
        fn cancel_timer(&self, id: TimerId);

        /// Returns `true` if this clock uses real system time.
        ///
        /// Used internally to determine whether to submit io_uring timeout SQEs
        /// (real clock) or register virtual timers (virtual clock).
        fn is_real(&self) -> bool {
            true
        }
    }

    /// Opaque identifier for a registered virtual timer.
    ///
    /// Returned by [`Clock::register_timer()`] and used with [`Clock::cancel_timer()`]
    /// to cancel pending timers.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct TimerId(pub(crate) u64);

    /// Real system clock implementation.
    ///
    /// This is the default clock used by kimojio runtimes. It delegates to
    /// [`Instant::now()`] for time queries and uses io_uring timeout SQEs for
    /// timer operations. Timer registration and cancellation are no-ops since
    /// real timers are managed by the kernel.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use kimojio::clock::{Clock, SystemClock};
    ///
    /// let clock = SystemClock;
    /// let now = clock.now();
    /// println!("Current time: {:?}", now);
    /// ```
    pub struct SystemClock;

    impl Clock for SystemClock {
        fn now(&self) -> Instant {
            Instant::now()
        }

        fn register_timer(&self, _deadline: Instant, _waker: Waker) -> TimerId {
            TimerId(0)
        }

        fn cancel_timer(&self, _id: TimerId) {}

        fn is_real(&self) -> bool {
            true
        }
    }

    /// Virtual clock for deterministic test timing.
    ///
    /// `VirtualClock` maintains a user-controlled notion of "now" that does not
    /// advance automatically. Instead, test code explicitly advances time via
    /// [`advance()`](VirtualClock::advance) or [`advance_to()`](VirtualClock::advance_to),
    /// causing any timers whose deadlines have been reached to fire immediately.
    ///
    /// This enables tests that involve timeouts, sleeps, and deadlines to run in
    /// microseconds instead of waiting real wall-clock time, while maintaining
    /// deterministic ordering guarantees.
    ///
    /// # Timer Ordering
    ///
    /// When advancing past multiple deadlines simultaneously, timers fire in
    /// deadline order (earliest first). Timers with equal deadlines fire in
    /// registration order (earliest registered first), providing fully
    /// deterministic behavior.
    ///
    /// # Thread Safety
    ///
    /// `VirtualClock` is `!Send` and `!Sync`, matching kimojio's single-threaded
    /// runtime model. It uses `Rc<RefCell<>>` internally and can be cheaply cloned
    /// to share between the runtime and test code.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use std::time::{Duration, Instant};
    /// use kimojio::clock::VirtualClock;
    ///
    /// let clock = VirtualClock::new(Instant::now());
    /// let start = clock.now();
    ///
    /// // Time doesn't advance on its own
    /// assert_eq!(clock.now(), start);
    ///
    /// // Advance time explicitly
    /// clock.advance(Duration::from_secs(10));
    /// assert_eq!(clock.now(), start + Duration::from_secs(10));
    /// ```
    #[derive(Clone)]
    pub struct VirtualClock {
        state: Rc<RefCell<VirtualClockState>>,
    }

    struct VirtualClockState {
        current: Instant,
        timers: BinaryHeap<VirtualTimer>,
        next_timer_id: u64,
    }

    struct VirtualTimer {
        id: TimerId,
        deadline: Instant,
        waker: Waker,
    }

    // Ordering: min-heap by (deadline, id) for deterministic tie-breaking
    impl PartialEq for VirtualTimer {
        fn eq(&self, other: &Self) -> bool {
            self.deadline == other.deadline && self.id == other.id
        }
    }

    impl Eq for VirtualTimer {}

    impl PartialOrd for VirtualTimer {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    impl Ord for VirtualTimer {
        fn cmp(&self, other: &Self) -> Ordering {
            // Reverse ordering for min-heap: smallest (deadline, id) has highest priority
            other
                .deadline
                .cmp(&self.deadline)
                .then_with(|| other.id.0.cmp(&self.id.0))
        }
    }

    impl VirtualClock {
        /// Creates a new virtual clock starting at the given instant.
        ///
        /// # Examples
        ///
        /// ```rust,no_run
        /// use std::time::Instant;
        /// use kimojio::clock::VirtualClock;
        ///
        /// let clock = VirtualClock::new(Instant::now());
        /// ```
        pub fn new(start: Instant) -> Self {
            Self {
                state: Rc::new(RefCell::new(VirtualClockState {
                    current: start,
                    timers: BinaryHeap::new(),
                    next_timer_id: 1,
                })),
            }
        }

        /// Returns the current virtual time.
        ///
        /// This does not advance; it returns whatever time was last set via
        /// [`advance()`](Self::advance) or [`advance_to()`](Self::advance_to).
        pub fn now(&self) -> Instant {
            self.state.borrow().current
        }

        /// Advances virtual time by the given duration.
        ///
        /// All timers with deadlines at or before the new current time are fired
        /// (their wakers are called) in deadline order. Returns the number of
        /// timers that fired.
        ///
        /// # Examples
        ///
        /// ```rust,no_run
        /// use std::time::{Duration, Instant};
        /// use kimojio::clock::VirtualClock;
        ///
        /// let clock = VirtualClock::new(Instant::now());
        /// let fired = clock.advance(Duration::from_secs(5));
        /// assert_eq!(fired, 0); // No timers registered
        /// ```
        pub fn advance(&self, duration: Duration) -> usize {
            let new_time = self.state.borrow().current + duration;
            self.advance_to(new_time)
        }

        /// Advances virtual time to a specific instant.
        ///
        /// All timers with deadlines at or before `target` are fired in deadline
        /// order. If `target` is before the current time, no timers fire and the
        /// clock is not moved backward.
        ///
        /// Returns the number of timers that fired.
        pub fn advance_to(&self, target: Instant) -> usize {
            let mut state = self.state.borrow_mut();
            if target > state.current {
                state.current = target;
            }

            let mut fired = 0;
            while let Some(timer) = state.timers.peek() {
                if timer.deadline <= state.current {
                    let timer = state.timers.pop().unwrap();
                    timer.waker.wake();
                    fired += 1;
                } else {
                    break;
                }
            }
            fired
        }

        /// Returns the deadline of the next pending timer, if any.
        ///
        /// Useful for stepping through time one timer at a time in tests:
        ///
        /// ```rust,no_run
        /// # use std::time::{Duration, Instant};
        /// # use kimojio::clock::VirtualClock;
        /// # let clock = VirtualClock::new(Instant::now());
        /// while let Some(next) = clock.next_deadline() {
        ///     clock.advance_to(next);
        ///     // Check state after each timer fires
        /// }
        /// ```
        pub fn next_deadline(&self) -> Option<Instant> {
            self.state.borrow().timers.peek().map(|t| t.deadline)
        }

        /// Returns the number of pending (unfired) timers.
        pub fn pending_timers(&self) -> usize {
            self.state.borrow().timers.len()
        }
    }

    impl Clock for VirtualClock {
        fn now(&self) -> Instant {
            self.state.borrow().current
        }

        fn register_timer(&self, deadline: Instant, waker: Waker) -> TimerId {
            let mut state = self.state.borrow_mut();
            let id = TimerId(state.next_timer_id);
            state.next_timer_id += 1;
            state.timers.push(VirtualTimer {
                id,
                deadline,
                waker,
            });
            id
        }

        fn cancel_timer(&self, id: TimerId) {
            let mut state = self.state.borrow_mut();
            state.timers.retain(|t| t.id != id);
        }

        fn is_real(&self) -> bool {
            false
        }
    }

    /// A future that completes when the virtual clock reaches the specified deadline.
    ///
    /// Created internally by [`sleep()`](crate::operations::sleep) and
    /// [`sleep_until()`](crate::operations::sleep_until) when a virtual clock
    /// is active. Registers a timer with the clock on first poll and resolves
    /// when virtual time is advanced past the deadline.
    ///
    /// Implements [`Drop`] to cancel the registered timer if the future is
    /// dropped before completion (FR-010: timer cancellation on drop).
    pub(crate) struct VirtualSleepFuture {
        deadline: Instant,
        clock: Rc<dyn Clock>,
        timer_id: Option<TimerId>,
        completed: bool,
    }

    impl VirtualSleepFuture {
        pub(crate) fn new(deadline: Instant, clock: Rc<dyn Clock>) -> Self {
            Self {
                deadline,
                clock,
                timer_id: None,
                completed: false,
            }
        }

        pub(crate) fn cancel(&mut self) {
            if let Some(id) = self.timer_id.take() {
                self.clock.cancel_timer(id);
            }
            self.completed = true;
        }

        pub(crate) fn is_terminated(&self) -> bool {
            self.completed
        }
    }

    impl std::future::Future for VirtualSleepFuture {
        type Output = Result<(), rustix_uring::Errno>;

        fn poll(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            if self.completed {
                panic!("polled after completion");
            }

            if self.clock.now() >= self.deadline {
                self.completed = true;
                if let Some(id) = self.timer_id.take() {
                    self.clock.cancel_timer(id);
                }
                return std::task::Poll::Ready(Ok(()));
            }

            // Cancel old timer and re-register with potentially updated waker
            if let Some(id) = self.timer_id.take() {
                self.clock.cancel_timer(id);
            }
            let id = self.clock.register_timer(self.deadline, cx.waker().clone());
            self.timer_id = Some(id);
            std::task::Poll::Pending
        }
    }

    impl Drop for VirtualSleepFuture {
        fn drop(&mut self) {
            if let Some(id) = self.timer_id.take() {
                self.clock.cancel_timer(id);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        use std::task::{RawWaker, RawWakerVTable};

        // Simple waker that increments a counter when woken
        fn counting_waker(counter: Arc<AtomicUsize>) -> Waker {
            fn clone_fn(data: *const ()) -> RawWaker {
                let counter = unsafe { Arc::from_raw(data as *const AtomicUsize) };
                let cloned = counter.clone();
                std::mem::forget(counter); // don't decrement ref count
                RawWaker::new(Arc::into_raw(cloned) as *const (), &VTABLE)
            }
            fn wake_fn(data: *const ()) {
                let counter = unsafe { Arc::from_raw(data as *const AtomicUsize) };
                counter.fetch_add(1, AtomicOrdering::SeqCst);
            }
            fn wake_by_ref_fn(data: *const ()) {
                let counter = unsafe { &*(data as *const AtomicUsize) };
                counter.fetch_add(1, AtomicOrdering::SeqCst);
            }
            fn drop_fn(data: *const ()) {
                unsafe {
                    drop(Arc::from_raw(data as *const AtomicUsize));
                }
            }
            static VTABLE: RawWakerVTable =
                RawWakerVTable::new(clone_fn, wake_fn, wake_by_ref_fn, drop_fn);
            let raw = RawWaker::new(Arc::into_raw(counter) as *const (), &VTABLE);
            unsafe { Waker::from_raw(raw) }
        }

        #[test]
        fn virtual_clock_initial_time() {
            let start = Instant::now();
            let clock = VirtualClock::new(start);
            assert_eq!(clock.now(), start);
        }

        #[test]
        fn advance_with_no_timers_returns_zero() {
            let clock = VirtualClock::new(Instant::now());
            assert_eq!(clock.advance(Duration::from_secs(10)), 0);
        }

        #[test]
        fn advance_by_zero_fires_no_timers() {
            let clock = VirtualClock::new(Instant::now());
            let counter = Arc::new(AtomicUsize::new(0));
            let waker = counting_waker(counter.clone());
            clock.register_timer(clock.now() + Duration::from_secs(1), waker);
            assert_eq!(clock.advance(Duration::ZERO), 0);
            assert_eq!(counter.load(AtomicOrdering::SeqCst), 0);
        }

        #[test]
        fn advance_fires_expired_timers() {
            let start = Instant::now();
            let clock = VirtualClock::new(start);
            let counter = Arc::new(AtomicUsize::new(0));
            let waker = counting_waker(counter.clone());
            clock.register_timer(start + Duration::from_secs(5), waker);

            assert_eq!(clock.advance(Duration::from_secs(5)), 1);
            assert_eq!(counter.load(AtomicOrdering::SeqCst), 1);
        }

        #[test]
        fn advance_does_not_fire_future_timers() {
            let start = Instant::now();
            let clock = VirtualClock::new(start);
            let counter = Arc::new(AtomicUsize::new(0));
            let waker = counting_waker(counter.clone());
            clock.register_timer(start + Duration::from_secs(10), waker);

            assert_eq!(clock.advance(Duration::from_secs(3)), 0);
            assert_eq!(counter.load(AtomicOrdering::SeqCst), 0);
        }

        #[test]
        fn timers_fire_in_deadline_order() {
            let start = Instant::now();
            let clock = VirtualClock::new(start);

            let counters: Vec<_> = (0..3).map(|_| Arc::new(AtomicUsize::new(0))).collect();

            // Register at 5s, 10s, 7s
            for (secs, i) in [(5, 0), (10, 1), (7, 2)] {
                clock.register_timer(
                    start + Duration::from_secs(secs),
                    counting_waker(counters[i].clone()),
                );
            }

            // Advance past all — should fire 3 timers
            assert_eq!(clock.advance(Duration::from_secs(15)), 3);
            for counter in &counters {
                assert_eq!(counter.load(AtomicOrdering::SeqCst), 1);
            }
        }

        #[test]
        fn equal_deadline_fires_in_registration_order() {
            let start = Instant::now();
            let clock = VirtualClock::new(start);
            let deadline = start + Duration::from_secs(5);

            let counters: Vec<_> = (0..3).map(|_| Arc::new(AtomicUsize::new(0))).collect();

            // Register 3 timers with same deadline
            for counter in &counters {
                let waker = counting_waker(counter.clone());
                clock.register_timer(deadline, waker);
            }

            assert_eq!(clock.advance(Duration::from_secs(5)), 3);
            // All should have been woken
            for counter in &counters {
                assert_eq!(counter.load(AtomicOrdering::SeqCst), 1);
            }
        }

        #[test]
        fn cancel_timer_prevents_firing() {
            let start = Instant::now();
            let clock = VirtualClock::new(start);
            let counter = Arc::new(AtomicUsize::new(0));
            let waker = counting_waker(counter.clone());

            let id = clock.register_timer(start + Duration::from_secs(5), waker);
            clock.cancel_timer(id);

            assert_eq!(clock.advance(Duration::from_secs(10)), 0);
            assert_eq!(counter.load(AtomicOrdering::SeqCst), 0);
        }

        #[test]
        fn cancel_nonexistent_timer_is_noop() {
            let clock = VirtualClock::new(Instant::now());
            clock.cancel_timer(TimerId(999)); // Should not panic
        }

        #[test]
        fn next_deadline_returns_earliest() {
            let start = Instant::now();
            let clock = VirtualClock::new(start);
            let counter = Arc::new(AtomicUsize::new(0));

            clock.register_timer(
                start + Duration::from_secs(10),
                counting_waker(counter.clone()),
            );
            clock.register_timer(
                start + Duration::from_secs(3),
                counting_waker(counter.clone()),
            );
            clock.register_timer(
                start + Duration::from_secs(7),
                counting_waker(counter.clone()),
            );

            assert_eq!(clock.next_deadline(), Some(start + Duration::from_secs(3)));
        }

        #[test]
        fn next_deadline_none_when_empty() {
            let clock = VirtualClock::new(Instant::now());
            assert_eq!(clock.next_deadline(), None);
        }

        #[test]
        fn advance_to_specific_instant() {
            let start = Instant::now();
            let clock = VirtualClock::new(start);
            let counter = Arc::new(AtomicUsize::new(0));

            clock.register_timer(
                start + Duration::from_secs(5),
                counting_waker(counter.clone()),
            );
            clock.register_timer(
                start + Duration::from_secs(10),
                counting_waker(counter.clone()),
            );

            let target = start + Duration::from_secs(7);
            assert_eq!(clock.advance_to(target), 1);
            assert_eq!(clock.now(), target);
            assert_eq!(counter.load(AtomicOrdering::SeqCst), 1);
        }

        #[test]
        fn advance_to_before_current_time_is_noop() {
            let start = Instant::now();
            let clock = VirtualClock::new(start);
            clock.advance(Duration::from_secs(10));

            let before = start + Duration::from_secs(5);
            assert_eq!(clock.advance_to(before), 0);
            assert_eq!(clock.now(), start + Duration::from_secs(10));
        }

        #[test]
        fn pending_timers_count() {
            let start = Instant::now();
            let clock = VirtualClock::new(start);
            let counter = Arc::new(AtomicUsize::new(0));

            assert_eq!(clock.pending_timers(), 0);

            clock.register_timer(
                start + Duration::from_secs(5),
                counting_waker(counter.clone()),
            );
            clock.register_timer(
                start + Duration::from_secs(10),
                counting_waker(counter.clone()),
            );
            assert_eq!(clock.pending_timers(), 2);

            clock.advance(Duration::from_secs(7));
            assert_eq!(clock.pending_timers(), 1);
        }

        #[test]
        fn system_clock_is_real() {
            let clock = SystemClock;
            assert!(clock.is_real());
        }

        #[test]
        fn virtual_clock_is_not_real() {
            let clock = VirtualClock::new(Instant::now());
            assert!(!Clock::is_real(&clock));
        }

        #[test]
        fn system_clock_now_returns_real_time() {
            let clock = SystemClock;
            let before = Instant::now();
            let now = clock.now();
            let after = Instant::now();
            assert!(now >= before && now <= after);
        }

        #[test]
        fn past_deadline_timer_fires_on_next_advance() {
            let start = Instant::now();
            let clock = VirtualClock::new(start + Duration::from_secs(10));
            let counter = Arc::new(AtomicUsize::new(0));

            // Register timer with deadline in the "past" (before current virtual time)
            clock.register_timer(
                start + Duration::from_secs(5),
                counting_waker(counter.clone()),
            );

            // Timer should not fire during registration
            assert_eq!(counter.load(AtomicOrdering::SeqCst), 0);

            // Timer fires on next advance — deadline (start+5) <= current (start+10)
            assert_eq!(clock.advance(Duration::ZERO), 1);
            assert_eq!(counter.load(AtomicOrdering::SeqCst), 1);
        }
    }
}

#[cfg(feature = "virtual-clock")]
pub use virtual_clock::{Clock, SystemClock, TimerId, VirtualClock};

#[cfg(feature = "virtual-clock")]
pub(crate) use virtual_clock::VirtualSleepFuture;
