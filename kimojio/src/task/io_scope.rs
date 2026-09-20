// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! Live registrations for one I/O scope.
//!
//! Each registry owns one strong reference per captured pending operation or wait.
//! An original CQE removes its I/O entry, even before the caller consumes the result.
//! Cancellation before submission also removes the entry.
//! A submitted cancellation request does not remove it.
//! Wait completion, observed cancellation, and future drop remove all memberships.
//! A wake alone leaves the wait registered.
//!
//! Registry identity remains stable across scope installation, suspension, and restoration.
//! Strong entries prevent pointer reuse while a key exists.
//! Weak reverse memberships avoid cycles and permit direct removal without history scans.
//! A canceled wait retry creates a new waiter, without memberships from its previous generation.
//! Zero or one reverse membership needs no separate allocation.
//! A second membership creates a vector of weak references for all captured scopes.
//! Retirement detaches the entire membership value before it visits registries.
//!
//! The per-scope map operations use expected amortized constant time.
//! Repeated pending polls do not append duplicate entries.
//! Each map shrinks when its capacity exceeds both 64 slots and four times its live length.
//! The target after shrinking is the greater of 32 slots and twice its live length.
//! Hash-table allocation granularity adds amortized slack, not operation-history storage.
//! Explicit cancellation snapshots only current registrations.
//!
//! The completion pool has its separate existing bound of 4096 records.
//! A cancel ACK retains a separate target owner until its genuine CQE.
//! Thus neither original/ACK ordering permits premature reuse of a cancellation target.
//! Waker callbacks and final resource destruction occur outside TaskState and registry borrows.
//! The ownership-order test controls ACK-owner release around genuine NOP completions.
//! It does not force kernel CQE order.

use std::{
    collections::HashMap,
    hash::Hash,
    rc::{Rc, Weak},
};

use crate::{Completion, MutInPlaceCell, async_event::WaitData, task_ref::wake_task};

use super::TaskState;

#[derive(Clone, Default)]
pub(crate) struct IoScopeCompletions {
    registry: Option<Rc<IoScopeRegistry>>,
}

#[derive(Default)]
pub(crate) struct IoScopeRegistry {
    entries: MutInPlaceCell<Entries>,
}

#[derive(Debug, Default)]
pub(crate) enum WaitScopes {
    #[default]
    Empty,
    One(Weak<IoScopeRegistry>),
    Many(Vec<Weak<IoScopeRegistry>>),
}

impl WaitScopes {
    pub(crate) fn push(&mut self, scope: Weak<IoScopeRegistry>) {
        match self {
            Self::Empty => *self = Self::One(scope),
            Self::One(_) => {
                let mut scopes = Vec::with_capacity(2);
                let Self::One(first) = std::mem::take(self) else {
                    unreachable!()
                };
                scopes.push(first);
                scopes.push(scope);
                *self = Self::Many(scopes);
            }
            Self::Many(scopes) => scopes.push(scope),
        }
    }

    pub(crate) fn retire(self, wait: *const WaitData) {
        let retire = |scope: Weak<IoScopeRegistry>| {
            if let Some(scope) = scope.upgrade() {
                scope.retire_wait(wait);
            }
        };
        match self {
            Self::Empty => {}
            Self::One(scope) => retire(scope),
            Self::Many(scopes) => scopes.into_iter().for_each(retire),
        }
    }
}

#[derive(Default)]
struct Entries {
    completions: HashMap<*const Completion, Rc<Completion>>,
    waits: HashMap<*const WaitData, Rc<WaitData>>,
}

fn shrink<K: Eq + Hash, V>(entries: &mut HashMap<K, V>) {
    // Geometric shrinking avoids keeping a historical concurrency peak and
    // amortizes rehashing across removals. Small registries retain their buckets.
    if entries.capacity() > 64.max(entries.len().saturating_mul(4)) {
        entries.shrink_to(32.max(entries.len().saturating_mul(2)));
    }
}

impl IoScopeRegistry {
    pub(crate) fn register_io(self: &Rc<Self>, completion: &Rc<Completion>) {
        self.entries.use_mut(|entries| {
            assert!(
                entries
                    .completions
                    .insert(Rc::as_ptr(completion), completion.clone())
                    .is_none()
            );
        });
        completion.scope.use_mut(|scope| {
            assert!(scope.is_none(), "completion already belongs to a scope");
            *scope = Some(Rc::downgrade(self));
        });
    }

    pub(crate) fn register_wait(self: &Rc<Self>, wait: &Rc<WaitData>) {
        let inserted = self.entries.use_mut(|entries| {
            if let std::collections::hash_map::Entry::Vacant(entry) =
                entries.waits.entry(Rc::as_ptr(wait))
            {
                entry.insert(wait.clone());
                true
            } else {
                false
            }
        });
        if inserted {
            wait.scopes
                .use_mut(|scopes| scopes.push(Rc::downgrade(self)));
        }
    }

    pub(crate) fn retire_io(&self, key: *const Completion) {
        let retired = self.entries.use_mut(|entries| {
            let retired = entries.completions.remove(&key);
            shrink(&mut entries.completions);
            retired
        });
        // The CQE handler, original future, or cancellation snapshot still owns
        // this completion. This cannot destroy its resources under TaskState.
        debug_assert!(
            retired
                .as_ref()
                .is_none_or(|item| Rc::strong_count(item) > 1)
        );
        drop(retired);
    }

    pub(crate) fn retire_wait(&self, key: *const WaitData) {
        let retired = self.entries.use_mut(|entries| {
            let retired = entries.waits.remove(&key);
            shrink(&mut entries.waits);
            retired
        });
        drop(retired);
    }

    fn has_io(&self) -> bool {
        self.entries
            .use_mut(|entries| !entries.completions.is_empty())
    }

    fn is_empty(&self) -> bool {
        self.entries
            .use_mut(|entries| entries.completions.is_empty() && entries.waits.is_empty())
    }

    fn cancel(&self) {
        let (completions, waits): (Vec<_>, Vec<_>) = self.entries.use_mut(|entries| {
            (
                entries.completions.values().cloned().collect(),
                entries.waits.values().cloned().collect(),
            )
        });
        if !completions.is_empty() {
            let mut task_state = TaskState::get();
            for completion in &completions {
                completion.cancel(&mut task_state);
            }
            task_state = crate::runtime::submit_and_complete_io_all(task_state, true);
            drop(task_state);
        }
        // Wakers and completion resources can reenter the runtime on wake/drop.
        // No registry or TaskState borrow can span these operations.
        drop(completions);
        for wait in waits {
            if !self
                .entries
                .use_mut(|entries| entries.waits.contains_key(&Rc::as_ptr(&wait)))
            {
                continue;
            }
            wait.canceled.set(true);
            let waker = wait.waker.use_mut(Option::take);
            if let Some(waker) = waker {
                let task_state = wake_task(TaskState::get(), waker);
                drop(task_state);
            }
        }
    }
}

impl IoScopeCompletions {
    pub(crate) fn registry(&mut self) -> Rc<IoScopeRegistry> {
        self.registry
            .get_or_insert_with(|| Rc::new(IoScopeRegistry::default()))
            .clone()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.registry
            .as_ref()
            .is_none_or(|registry| registry.is_empty())
    }

    pub(crate) fn has_io(&self) -> bool {
        self.registry
            .as_ref()
            .is_some_and(|registry| registry.has_io())
    }

    pub(crate) fn cancel(&self) {
        if let Some(registry) = &self.registry {
            registry.cancel();
        }
    }
}

#[cfg(test)]
mod tests;
