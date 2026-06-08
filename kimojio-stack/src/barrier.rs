// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::RefCell;
use std::fmt;

use crate::{RuntimeContext, Waiters};

/// A barrier that blocks stackful coroutines until `n` participants arrive.
pub struct Barrier {
    parties: usize,
    state: RefCell<State>,
}

impl Barrier {
    /// Creates a barrier for `parties` participants.
    pub fn new(parties: usize) -> Self {
        assert!(parties != 0, "barrier must have at least one participant");
        Self {
            parties,
            state: RefCell::new(State {
                arrived: 0,
                generation: 0,
                waiters: Waiters::default(),
            }),
        }
    }

    /// Waits until all participants in this generation arrive.
    pub fn wait(&self, cx: &RuntimeContext<'_>) -> BarrierWaitResult {
        let generation = {
            let mut state = self.state.borrow_mut();
            let generation = state.generation;
            state.arrived += 1;

            if state.arrived == self.parties {
                state.arrived = 0;
                state.generation = state.generation.wrapping_add(1);
                state.waiters.wake_all();
                return BarrierWaitResult { leader: true };
            }

            generation
        };

        loop {
            {
                let mut state = self.state.borrow_mut();
                if state.generation != generation {
                    return BarrierWaitResult { leader: false };
                }

                if let Some(waiter) = cx.waiter() {
                    state.waiters.push(waiter);
                }
            }
            cx.park();
        }
    }
}

impl fmt::Debug for Barrier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Barrier")
            .field("parties", &self.parties)
            .finish_non_exhaustive()
    }
}

/// Result from [`Barrier::wait`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BarrierWaitResult {
    leader: bool,
}

impl BarrierWaitResult {
    /// Returns whether this participant released the barrier generation.
    pub fn is_leader(self) -> bool {
        self.leader
    }
}

struct State {
    arrived: usize,
    generation: u64,
    waiters: Waiters,
}

#[cfg(test)]
mod tests {
    use crate::{Barrier, Runtime};

    #[test]
    fn barrier_releases_all_participants() {
        let barrier = Barrier::new(3);
        let mut runtime = Runtime::new();

        let leaders = runtime.block_on(|cx| {
            cx.scope(|scope| {
                let first = scope.spawn(|cx| barrier.wait(cx).is_leader() as usize);
                let second = scope.spawn(|cx| barrier.wait(cx).is_leader() as usize);
                let third = scope.spawn(|cx| barrier.wait(cx).is_leader() as usize);

                first.join(cx) + second.join(cx) + third.join(cx)
            })
        });

        assert_eq!(leaders, 1);
    }
}
