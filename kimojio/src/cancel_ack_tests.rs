// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
use super::*;
use crate::{CompletionResources, MutInPlaceCell, scope_registry::IoScopeCompletions};
use rustix::io_uring::io_uring_user_data;
use rustix_uring::opcode;
use std::task::Waker;

fn submitted() -> Completion {
    Completion {
        state: MutInPlaceCell::new(CompletionState::Submitted {
            waker: Waker::noop().clone(),
            activity_id: Default::default(),
            tag: 0,
            canceled: false,
        }),
        scope: Default::default(),
        owned_resources: CompletionResources::None,
        timespec: Default::default(),
        tag: 0,
        task_index: 0,
        iopoll: false,
    }
}

#[crate::test]
async fn cancel_ack_pins_identity_after_original_cqe_until_ack_is_reaped() {
    let old_pool = std::mem::take(&mut TaskState::get().completion_pool);
    drop(old_pool);
    let mut scope = IoScopeCompletions::default();
    let io = Rc::new(submitted());
    scope.register_io(&io);
    let target = Rc::as_ptr(&io);
    let original = Rc::into_raw(io.clone()).cast_mut().cast();
    let entry = opcode::Nop::new()
        .build()
        .user_data(io_uring_user_data::from_ptr(original));
    let mut state = TaskState::get();
    let baseline = state.stats.in_flight_io.get();
    state.submit(&[entry.into()]);
    state.stats.increment_in_flight_io(1);
    // Put the actual original CQE in the queue before queuing the cancel SQE.
    // Do not submit the latter until the original owner has been released.
    state.ring.submit_and_wait(1);
    io.cancel(&mut state);
    assert_eq!(Rc::strong_count(&io), 4, "future, scope, original, ACK");
    state = process_completions(
        state,
        RingEventTraceInfo {
            start_time: 0,
            ring_tag: 0,
            submissions: 0,
            in_flight_io: baseline + 2,
            want: false,
            iopoll: false,
        },
    );
    io.state.use_mut(|value| {
        assert!(matches!(
            value,
            CompletionState::Completed { result: Ok(0), .. }
        ))
    });
    assert!(scope.is_empty(), "original CQE retired the registration");
    assert_eq!(
        state.stats.in_flight_io.get(),
        baseline + 1,
        "only ACK remains in flight"
    );
    assert_eq!(Rc::strong_count(&io), 2, "future and ACK pin");
    let unpooled = state.return_completion(io);
    assert!(
        unpooled.is_some(),
        "cannot pool while an ACK pins the target"
    );
    drop(state);
    drop(unpooled);

    let (fresh, previous) = TaskState::get().new_completion(submitted());
    drop(previous);
    assert_ne!(
        Rc::as_ptr(&fresh),
        target,
        "target identity must not be reused"
    );
    drop(fresh);
    for _ in 0..16384 {
        drop(submit_and_complete_io_all(TaskState::get(), true));
        if TaskState::get().stats.in_flight_io.get() == baseline {
            break;
        }
    }
    let state = TaskState::get();
    assert_eq!(state.stats.in_flight_io.get(), baseline);
    assert!(
        state
            .completion_pool
            .iter()
            .any(|io| Rc::as_ptr(io) == target),
        "ACK releases the last owner back to the pool"
    );
}
