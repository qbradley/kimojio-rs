// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
use super::*;
use crate::{CompletionResources, CompletionState, task::TaskState};
use intrusive_collections::LinkedListLink;
use rustix::io_uring::io_uring_user_data;
use rustix_uring::opcode;
use std::{cell::Cell, task::Waker};

fn completion() -> Rc<Completion> {
    Rc::new(Completion {
        state: MutInPlaceCell::new(CompletionState::Idle {
            entry: Some(opcode::Nop::new().build().into()),
            timespec: false,
        }),
        scope: Default::default(),
        owned_resources: CompletionResources::None,
        timespec: Default::default(),
        tag: 0,
        task_index: 0,
        iopoll: false,
    })
}

fn waiter() -> Rc<WaitData> {
    Rc::new(WaitData {
        activity_id: Default::default(),
        waker: MutInPlaceCell::new(Some(Waker::noop().clone())),
        task_id: 0,
        canceled: Cell::new(false),
        tag: 0,
        link: LinkedListLink::new(),
        scopes: Default::default(),
    })
}

fn check(scope: &IoScopeCompletions) {
    let Some(scope) = &scope.registry else { return };
    scope.completions.use_mut(|entries| {
        for (slot, completion) in entries.iter().enumerate() {
            completion.scope.0.use_mut(|registration| {
                let registration = registration.as_ref().unwrap();
                assert_eq!(registration.slot, slot);
                assert!(Rc::ptr_eq(&registration.scope.upgrade().unwrap(), scope));
            });
        }
    });
    scope.waits.use_mut(|entries| {
        for (slot, entry) in entries.iter().enumerate() {
            entry.wait.scopes.0.use_mut(|memberships| {
                let registration = memberships.get_mut(entry.reverse_slot);
                assert_eq!(registration.slot, slot);
                assert!(Rc::ptr_eq(&registration.scope.upgrade().unwrap(), scope));
            });
        }
    });
}

#[test]
fn pending_io_query_ignores_waiters_and_retained_snapshot_owners() {
    let mut scope = IoScopeCompletions::default();
    assert!(!scope.has_pending_io());
    assert!(
        scope.registry.is_none(),
        "a settlement query must not allocate a registry"
    );

    let wait = waiter();
    scope.register_wait(&wait);
    assert!(!scope.is_empty());
    assert!(
        !scope.has_pending_io(),
        "event waiters are not original I/O"
    );

    let io = completion();
    scope.register_io(&io);
    let snapshot = scope.snapshot();
    // Settlement must not inspect completion states, even when the entry is live.
    io.state.use_mut(|_| assert!(scope.has_pending_io()));
    // The query follows authoritative retirement, not temporary snapshot owners.
    // Genuine original/ACK CQE behavior is covered by the runtime test below.
    io.scope.retire();
    assert!(!scope.has_pending_io());
    assert_eq!(snapshot.completions.len(), 1);
    assert_eq!(
        Rc::strong_count(&io),
        2,
        "the snapshot still owns the retired I/O"
    );
    wait.scopes.retire();
    assert!(scope.is_empty());
}

#[test]
fn lazy_identity_and_inline_one_survive_scope_moves() {
    let scope = IoScopeCompletions::default();
    assert!(scope.registry.is_none());
    assert!(scope.snapshot().completions.is_empty());
    assert!(scope.registry.is_none());
    let mut scope = scope;
    let io = completion();
    let wait = waiter();
    scope.register_io(&io);
    scope.register_wait(&wait);
    let identity = Rc::as_ptr(scope.registry.as_ref().unwrap());
    // Moving the wrapper mirrors the task/future poll-time scope swaps.
    let mut saved = Some(scope);
    let mut moved = saved.take().unwrap();
    for _ in 0..4096 {
        moved.register_io(&io);
        moved.register_wait(&wait);
    }
    assert_eq!(identity, Rc::as_ptr(moved.registry.as_ref().unwrap()));
    let registry = moved.registry.as_ref().unwrap();
    registry.completions.use_mut(|entries| {
        assert_eq!(entries.len(), 1);
        assert_eq!(entries.spill.capacity(), 0);
    });
    registry.waits.use_mut(|entries| {
        assert_eq!(entries.len(), 1);
        assert_eq!(entries.spill.capacity(), 0);
    });
    wait.scopes.0.use_mut(|memberships| {
        assert_eq!(memberships.len(), 1);
        assert_eq!(memberships.spill.capacity(), 0);
    });
    let snapshot = moved.snapshot();
    assert_eq!(snapshot.completions.spill.capacity(), 0);
    assert_eq!(snapshot.waits.spill.capacity(), 0);
    io.scope.retire();
    wait.scopes.retire();
    assert!(moved.is_empty());
    // Snapshot owners survive retirement, independently of the registry.
    assert_eq!(Rc::strong_count(&io), 2);
    assert_eq!(Rc::strong_count(&wait), 2);
    drop(snapshot);
    assert_eq!(Rc::strong_count(&io), 1);
    assert_eq!(Rc::strong_count(&wait), 1);
}

#[test]
fn completion_swap_remove_repairs_first_middle_and_last_backlinks() {
    let mut scope = IoScopeCompletions::default();
    let ios: Vec<_> = (0..64).map(|_| completion()).collect();
    for io in &ios {
        scope.register_io(io);
    }
    // A coprime stride visits every original owner,
    // repeatedly removing entries that previous swap-removals moved.
    for index in (0..64).map(|index| index * 29 % 64) {
        ios[index].scope.retire();
        assert_eq!(Rc::strong_count(&ios[index]), 1);
        check(&scope);
    }
    assert!(scope.is_empty());
}

#[test]
fn multi_scope_waiter_repairs_both_dense_slots_and_reverse_slots() {
    let mut scopes: Vec<_> = (0..9).map(|_| IoScopeCompletions::default()).collect();
    let waits: Vec<_> = (0..13).map(|_| waiter()).collect();
    for (index, scope) in scopes.iter_mut().enumerate() {
        // Different insertion orders make each scope slot independent of the
        // waiter's reverse_slot; neither index can substitute for the other.
        for offset in 0..waits.len() {
            scope.register_wait(&waits[(offset + index) % waits.len()]);
        }
    }
    for scope in &scopes {
        check(scope);
    }
    for index in (0..13).map(|index| index * 5 % 13) {
        // Check intermediate removals, especially when membership slot zero
        // receives the last scope's membership and changes reverse_slot.
        while waits[index].scopes.retire_one() {
            for scope in &scopes {
                check(scope);
            }
        }
        assert_eq!(Rc::strong_count(&waits[index]), 1);
    }
    assert!(scopes.iter().all(IoScopeCompletions::is_empty));
}

#[test]
fn burst_retirement_retains_capacity_without_retaining_completed_owners() {
    let mut scope = IoScopeCompletions::default();
    let ios: Vec<_> = (0..4096).map(|_| completion()).collect();
    let waits: Vec<_> = (0..4096).map(|_| waiter()).collect();
    for (io, wait) in ios.iter().zip(&waits) {
        scope.register_io(io);
        scope.register_wait(wait);
    }
    let capacities = || {
        let registry = scope.registry.as_ref().unwrap();
        (
            registry
                .completions
                .use_mut(|entries| 1 + entries.spill.capacity()),
            registry
                .waits
                .use_mut(|entries| 1 + entries.spill.capacity()),
        )
    };
    let before = capacities();
    assert!(before.0 >= 4096 && before.1 >= 4096);
    for index in (0..4096).map(|index| index * 4051 % 4096) {
        ios[index].scope.retire();
        waits[index].scopes.retire();
    }
    assert!(scope.is_empty());
    assert_eq!(
        capacities(),
        before,
        "retirement must not shrink burst capacity"
    );
    assert!(ios.iter().all(|io| Rc::strong_count(io) == 1));
    assert!(waits.iter().all(|wait| Rc::strong_count(wait) == 1));
    for (io, wait) in ios.iter().zip(&waits) {
        scope.register_io(io);
        scope.register_wait(wait);
    }
    check(&scope);
    let registry = scope.registry.as_ref().unwrap();
    assert_eq!(
        registry
            .completions
            .use_mut(|entries| 1 + entries.spill.capacity()),
        before.0
    );
    assert_eq!(
        registry
            .waits
            .use_mut(|entries| 1 + entries.spill.capacity()),
        before.1
    );
    for (io, wait) in ios.iter().zip(&waits) {
        io.scope.retire();
        wait.scopes.retire();
    }
}

#[crate::test]
async fn real_cancel_ack_cannot_settle_or_retire_the_original() {
    let mut scope = IoScopeCompletions::default();
    let io = completion();
    scope.register_io(&io);
    io.state.use_mut(|state| {
        *state = CompletionState::Submitted {
            waker: Waker::noop().clone(),
            activity_id: Default::default(),
            tag: 0,
            canceled: false,
        }
    });
    let baseline = TaskState::get().stats.in_flight_io.get();
    {
        let mut state = TaskState::get();
        io.cancel(&mut state);
        assert_eq!(state.stats.in_flight_io.get(), baseline + 1);
        assert_eq!(Rc::strong_count(&io), 3, "future, registry, ACK pin");
        io.cancel(&mut state);
        assert_eq!(Rc::strong_count(&io), 3, "duplicate cancel adds no ACK pin");
        assert_eq!(state.stats.in_flight_io.get(), baseline + 1);
    }
    // Deliberately delay the original SQE. A genuine AsyncCancel NOTFOUND ACK
    // must leave the still-submitted original state and registration untouched.
    for _ in 0..16384 {
        drop(crate::runtime::submit_and_complete_io_all(
            TaskState::get(),
            true,
        ));
        if TaskState::get().stats.in_flight_io.get() == baseline {
            break;
        }
    }
    assert_eq!(TaskState::get().stats.in_flight_io.get(), baseline);
    io.state.use_mut(|state| {
        assert!(matches!(
            state,
            CompletionState::Submitted { canceled: true, .. }
        ))
    });
    assert!(!scope.is_empty());
    assert!(
        scope.has_pending_io(),
        "a cancellation ACK cannot settle original I/O"
    );
    assert_eq!(Rc::strong_count(&io), 2, "the ACK pin was released");
    check(&scope);

    let original = Rc::into_raw(io.clone()).cast_mut().cast();
    let entry = opcode::Nop::new()
        .build()
        .user_data(io_uring_user_data::from_ptr(original));
    {
        let mut state = TaskState::get();
        state.submit(&[entry.into()]);
        state.stats.increment_in_flight_io(1);
    }
    for _ in 0..16384 {
        drop(crate::runtime::submit_and_complete_io_all(
            TaskState::get(),
            true,
        ));
        if scope.is_empty() {
            break;
        }
    }
    assert!(scope.is_empty(), "only the original CQE retires its entry");
    assert!(!scope.has_pending_io());
    io.state.use_mut(|state| {
        assert!(matches!(
            state,
            CompletionState::Completed { result: Ok(0), .. }
        ))
    });
    assert_eq!(Rc::strong_count(&io), 1);
    assert_eq!(TaskState::get().stats.in_flight_io.get(), baseline);
}
