// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::{Cell, RefCell};
use std::fmt;

use crate::{RuntimeContext, Waitable, Waiters};

/// A cooperative notification primitive.
#[derive(Default)]
pub struct Notify {
    state: RefCell<State>,
}

impl Notify {
    /// Creates a notification primitive without a stored permit.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a waitable notification handle.
    pub fn notified(&self) -> Notified<'_> {
        Notified {
            notify: self,
            observed_generation: self.state.borrow().generation,
            consumed: Cell::new(false),
        }
    }

    /// Stores one permit and wakes one waiter.
    pub fn notify_one(&self) {
        let mut state = self.state.borrow_mut();
        state.permits = state
            .permits
            .checked_add(1)
            .expect("notify permit count overflow");
        state.waiters.wake_one();
    }

    /// Wakes every waiter currently observing this notification.
    pub fn notify_waiters(&self) {
        let mut state = self.state.borrow_mut();
        state.generation = state.generation.wrapping_add(1);
        state.waiters.wake_all();
    }
}

impl fmt::Debug for Notify {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notify").finish_non_exhaustive()
    }
}

impl fmt::Debug for Notified<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notified")
            .field("ready", &self.is_ready())
            .finish()
    }
}

/// A waitable notification.
pub struct Notified<'a> {
    notify: &'a Notify,
    observed_generation: u64,
    consumed: Cell<bool>,
}

impl Notified<'_> {
    /// Attempts to consume this notification without parking.
    pub fn try_wait(&self) -> bool {
        if self.consumed.get() {
            return true;
        }

        let mut state = self.notify.state.borrow_mut();
        if state.generation != self.observed_generation {
            self.consumed.set(true);
            return true;
        }

        if state.permits != 0 {
            state.permits -= 1;
            self.consumed.set(true);
            return true;
        }

        false
    }

    /// Waits for this notification.
    pub fn wait(&self, cx: &RuntimeContext<'_>) {
        loop {
            if self.try_wait() {
                return;
            }

            if let Some(waiter) = cx.waiter() {
                self.notify.state.borrow_mut().waiters.push(waiter);
            }
            cx.park();
        }
    }
}

impl Waitable for Notified<'_> {
    fn is_ready(&self) -> bool {
        if self.consumed.get() {
            return true;
        }

        let state = self.notify.state.borrow();
        state.generation != self.observed_generation || state.permits != 0
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>) {
        if !self.is_ready()
            && let Some(waiter) = cx.waiter()
        {
            self.notify.state.borrow_mut().waiters.push(waiter);
        }
    }
}

#[derive(Default)]
struct State {
    permits: usize,
    generation: u64,
    waiters: Waiters,
}

#[cfg(test)]
mod tests {
    use crate::{Notify, Runtime};

    #[test]
    fn notify_one_wakes_one_waiter() {
        let notify = Notify::new();
        let mut runtime = Runtime::new();

        let output = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let waiter = notify.notified();
                let task = scope.spawn(move |cx| {
                    waiter.wait(cx);
                    41
                });
                let notifier = scope.spawn(|_| {
                    notify.notify_one();
                    1
                });

                task.join(cx) + notifier.join(cx)
            })
        });

        assert_eq!(output, 42);
    }

    #[test]
    fn notify_waiters_releases_existing_observers() {
        let notify = Notify::new();
        let first = notify.notified();
        let second = notify.notified();

        notify.notify_waiters();

        assert!(first.try_wait());
        assert!(second.try_wait());
        assert!(!notify.notified().try_wait());
    }
}
