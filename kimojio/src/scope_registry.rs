// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! Live scope registrations. Owning dense entries have direct reverse indices;
//! retiring an entry never searches, and swap-removal repairs the moved backlink.
//! Scope identity is allocated lazily and remains stable across poll-time swaps.
//! Backlinks are weak: entries own operations, never the other way around.

use std::rc::{Rc, Weak};

use crate::{
    Completion, MutInPlaceCell, async_event::WaitData, task_ref::wake_task,
    task_state_cell::TaskStateCellRef,
};

/// A dense sequence with an allocation-free first entry. Spill capacity is kept
/// even after a burst retires: repeated workloads should not reallocate.
#[derive(Clone, Debug)]
struct InlineVec<T> {
    first: Option<T>,
    spill: Vec<T>,
}

impl<T> Default for InlineVec<T> {
    fn default() -> Self {
        Self {
            first: None,
            spill: Vec::new(),
        }
    }
}

impl<T> InlineVec<T> {
    fn len(&self) -> usize {
        usize::from(self.first.is_some()) + self.spill.len()
    }

    fn is_empty(&self) -> bool {
        self.first.is_none()
    }

    fn push(&mut self, value: T) {
        if self.first.is_none() {
            self.first = Some(value);
        } else {
            self.spill.push(value);
        }
    }

    fn get_mut(&mut self, index: usize) -> &mut T {
        if index == 0 {
            self.first.as_mut().unwrap()
        } else {
            &mut self.spill[index - 1]
        }
    }

    fn swap_remove(&mut self, index: usize) -> T {
        if index == 0 {
            let replacement = self.spill.pop();
            std::mem::replace(&mut self.first, replacement).unwrap()
        } else {
            self.spill.swap_remove(index - 1)
        }
    }

    fn pop(&mut self) -> Option<T> {
        self.spill.pop().or_else(|| self.first.take())
    }

    fn iter(&self) -> impl Iterator<Item = &T> {
        self.first.iter().chain(self.spill.iter())
    }
}

#[derive(Default)]
pub(crate) struct IoScopeCompletions {
    registry: Option<Rc<Registry>>,
}

#[derive(Default)]
struct Registry {
    completions: MutInPlaceCell<InlineVec<Rc<Completion>>>,
    waits: MutInPlaceCell<InlineVec<WaitEntry>>,
}

struct WaitEntry {
    wait: Rc<WaitData>,
    reverse_slot: usize,
}

/// Completion membership is singular: I/O belongs to its creation scope.
#[derive(Default)]
pub(crate) struct CompletionMembership(MutInPlaceCell<Option<CompletionRegistration>>);

struct CompletionRegistration {
    scope: Weak<Registry>,
    slot: usize,
}

/// The common single-scope waiter needs no membership allocation. A shared wait
/// may belong to arbitrarily many scopes. Dedup compares the first identity
/// directly; checking rare multi-scope registrations is explicitly O(k).
#[derive(Debug, Default)]
pub(crate) struct WaitMemberships(MutInPlaceCell<InlineVec<WaitRegistration>>);

#[derive(Debug)]
struct WaitRegistration {
    scope: Weak<Registry>,
    slot: usize,
}

impl IoScopeCompletions {
    fn registry(&mut self) -> &Rc<Registry> {
        self.registry.get_or_insert_with(Rc::default)
    }

    pub(crate) fn register_io(&mut self, completion: &Rc<Completion>) {
        completion.scope.0.use_mut(|membership| {
            if membership.is_some() {
                return;
            }
            let scope = self.registry();
            scope.completions.use_mut(|entries| {
                *membership = Some(CompletionRegistration {
                    scope: Rc::downgrade(scope),
                    slot: entries.len(),
                });
                entries.push(completion.clone());
            });
        });
    }

    pub(crate) fn register_wait(&mut self, wait: &Rc<WaitData>) {
        // A waker clone may cancel this generation reentrantly during poll.
        if wait.canceled.get() {
            return;
        }
        let scope = self.registry();
        wait.scopes.0.use_mut(|memberships| {
            if memberships
                .iter()
                .any(|entry| entry.scope.as_ptr() == Rc::as_ptr(scope))
            {
                return;
            }
            scope.waits.use_mut(|entries| {
                entries.push(WaitEntry {
                    wait: wait.clone(),
                    reverse_slot: memberships.len(),
                });
                memberships.push(WaitRegistration {
                    scope: Rc::downgrade(scope),
                    slot: entries.len() - 1,
                });
            });
        });
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.registry.as_ref().is_none_or(|scope| {
            scope.completions.use_mut(|entries| entries.is_empty())
                && scope.waits.use_mut(|entries| entries.is_empty())
        })
    }

    /// Whether this scope still has unsettled original I/O, including operations
    /// not yet submitted. Waiters and cancellation ACKs do not affect this query;
    /// only original-CQE retirement or pre-submission cancellation removes I/O.
    pub(crate) fn has_pending_io(&self) -> bool {
        self.registry
            .as_ref()
            .is_some_and(|scope| scope.completions.use_mut(|entries| !entries.is_empty()))
    }

    #[cfg(test)]
    pub(crate) fn test_completions(&self) -> Vec<Rc<Completion>> {
        self.snapshot().completions.iter().cloned().collect()
    }

    #[cfg(test)]
    pub(crate) fn test_waits(&self) -> Vec<Rc<WaitData>> {
        self.snapshot().waits.iter().cloned().collect()
    }

    /// Capture exactly the current owners before any callback or kernel drain.
    /// Entries stay live until settlement; the snapshot is independent of new
    /// registrations and recursive cancellation, including waiter retries.
    pub(crate) fn snapshot(&self) -> ScopeSnapshot {
        let mut snapshot = ScopeSnapshot::default();
        if let Some(scope) = &self.registry {
            snapshot.completions = scope.completions.use_mut(|entries| entries.clone());
            snapshot.waits = scope.waits.use_mut(|entries| InlineVec {
                first: entries.first.as_ref().map(|entry| entry.wait.clone()),
                // Exact-size collection allocates once for a large snapshot;
                // cloning an empty spill never allocates for the common case.
                spill: entries
                    .spill
                    .iter()
                    .map(|entry| entry.wait.clone())
                    .collect(),
            });
        }
        snapshot
    }
}

impl CompletionMembership {
    /// Called only once the original I/O settles (or idle I/O is canceled),
    /// never for an AsyncCancel ACK. The caller still owns the completion, so
    /// releasing the registry owner cannot invoke a resource destructor.
    pub(crate) fn retire(&self) {
        let Some(registration) = self.0.use_mut(Option::take) else {
            return;
        };
        if let Some(scope) = registration.scope.upgrade() {
            let removed = scope.completions.use_mut(|entries| {
                let removed = entries.swap_remove(registration.slot);
                if registration.slot < entries.len() {
                    entries
                        .get_mut(registration.slot)
                        .scope
                        .0
                        .use_mut(|membership| {
                            membership.as_mut().unwrap().slot = registration.slot;
                        });
                }
                removed
            });
            drop(removed);
        }
    }
}

impl WaitMemberships {
    /// Remove every membership of this generation without searching either
    /// side. Both swap-removals repair their moved entry's reverse index.
    pub(crate) fn retire(&self) {
        while self.retire_one() {}
    }

    fn retire_one(&self) -> bool {
        let Some(registration) = self.0.use_mut(|memberships| {
            if memberships.is_empty() {
                return None;
            }
            let removed = memberships.swap_remove(0);
            if let Some(moved) = memberships.first.as_ref()
                && let Some(scope) = moved.scope.upgrade()
            {
                scope
                    .waits
                    .use_mut(|entries| entries.get_mut(moved.slot).reverse_slot = 0);
            }
            Some(removed)
        }) else {
            return false;
        };
        if let Some(scope) = registration.scope.upgrade() {
            let removed = scope.waits.use_mut(|entries| {
                let removed = entries.swap_remove(registration.slot);
                if registration.slot < entries.len() {
                    let moved = entries.get_mut(registration.slot);
                    moved.wait.scopes.0.use_mut(|memberships| {
                        memberships.get_mut(moved.reverse_slot).slot = registration.slot;
                    });
                }
                removed
            });
            // The caller owns this waiter throughout retirement.
            drop(removed);
        }
        true
    }
}

#[cfg(test)]
#[path = "scope_registry_tests.rs"]
mod tests;

#[derive(Default)]
pub(crate) struct ScopeSnapshot {
    completions: InlineVec<Rc<Completion>>,
    waits: InlineVec<Rc<WaitData>>,
}

impl ScopeSnapshot {
    /// Request cancellation of this exact batch without waiting for all originals.
    /// The live registry retains unsettled I/O after snapshot owners are released.
    /// Destructors and wakers run outside registry and TaskState borrows.
    pub(crate) fn cancel<'a>(
        mut self,
        mut task_state: TaskStateCellRef<'a>,
    ) -> TaskStateCellRef<'a> {
        if self.completions.is_empty() && self.waits.is_empty() {
            return task_state;
        }
        task_state = crate::runtime::submit_and_complete_io_all(task_state, true);
        for completion in self.completions.iter() {
            completion.cancel(&mut task_state);
        }
        while let Some(wait) = self.waits.pop() {
            wait.canceled.set(true);
            wait.scopes.retire();
            if let Some(waker) = wait.waker.use_mut(Option::take) {
                task_state = wake_task(task_state, waker);
            }
            let cell = task_state.into_inner();
            drop(wait);
            task_state = cell.borrow_mut();
        }
        // Preserve the cancellation progress pass, then release each temporary
        // owner once. Original CQEs retire the authoritative live entries.
        task_state = crate::runtime::submit_and_complete_io_all(task_state, true);
        while let Some(completion) = self.completions.pop() {
            let unpooled = task_state.return_completion(completion);
            let cell = task_state.into_inner();
            drop(unpooled);
            task_state = cell.borrow_mut();
        }
        task_state
    }
}
