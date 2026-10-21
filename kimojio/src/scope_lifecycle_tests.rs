// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
use super::*;

#[crate::test]
async fn scope_exit_settles_many_originals_retained_outside_its_snapshot() {
    for explicitly_cancel_first in [false, true] {
        let (readers, peers): (Vec<_>, Vec<_>) = (0..64).map(|_| crate::pipe::bipipe()).unzip();
        let mut buffers = [[0; 1]; 64];
        let mut pending = operations::io_scope(async || {
            let mut reads: Vec<_> = readers
                .iter()
                .zip(buffers.iter_mut())
                .map(|(reader, buffer)| operations::read(reader, buffer))
                .collect();
            futures::future::poll_fn(|cx| {
                for read in &mut reads {
                    assert!(std::pin::Pin::new(read).poll(cx).is_pending());
                }
                Poll::Ready(())
            })
            .await;
            if explicitly_cancel_first {
                operations::io_scope_cancel();
            }
            // Keep the real futures and their buffer borrows outside the scope.
            // Exit must settle every original, not just its cancellation request.
            reads
        })
        .await;
        for read in &mut pending {
            assert_eq!(
                futures::poll!(std::pin::Pin::new(read)),
                Poll::Ready(Err(crate::Errno::CANCELED)),
                "scope exit returned before an original CQE was processed"
            );
        }
        drop(pending);
        assert!(buffers.iter().all(|buffer| *buffer == [0]));
        drop((readers, peers));
    }
}

#[crate::test]
async fn long_scope_retires_4096_completed_operations_and_deduplicates_waits() {
    operations::io_scope(async || {
        let task = TaskState::get().get_current_task();
        let event = AsyncEvent::new();
        for _ in 0..4096 {
            operations::nop().await.unwrap();
            task.io_scope_completions.use_mut(|scope| {
                assert!(
                    scope.as_ref().unwrap().is_empty(),
                    "no completed I/O history"
                );
            });
            let mut wait = Box::pin(event.wait());
            for _ in 0..4 {
                assert!(futures::poll!(wait.as_mut()).is_pending());
            }
            let weak = task.io_scope_completions.use_mut(|scope| {
                let waits = scope.as_ref().unwrap().test_waits();
                assert_eq!(waits.len(), 1, "repeated pending polls are deduplicated");
                Rc::downgrade(&waits[0])
            });
            drop(wait);
            assert!(weak.upgrade().is_none(), "dropped wait was not retained");
            task.io_scope_completions
                .use_mut(|scope| assert!(scope.as_ref().unwrap().is_empty()));
        }
        // A successful ready poll retires even if the future itself is kept.
        let mut wait = Box::pin(event.wait());
        assert!(futures::poll!(wait.as_mut()).is_pending());
        event.set();
        assert_eq!(futures::poll!(wait.as_mut()), Poll::Ready(Ok(())));
        task.io_scope_completions
            .use_mut(|scope| assert!(scope.as_ref().unwrap().is_empty()));
    })
    .await;
}

#[crate::test]
async fn recursive_cancel_snapshots_new_work_without_extending_outer_snapshot() {
    operations::io_scope(async || {
        let task = TaskState::get().get_current_task();
        let event = AsyncEvent::new();
        let mut wait = Box::pin(event.wait());
        let pending = Rc::new(RefCell::new(None));
        let calls = Rc::new(Cell::new(0));
        let on_wake: Rc<dyn Fn()> = {
            let pending = pending.clone();
            Rc::new(move || {
                let mut canceled = Box::pin(operations::nop());
                operations::io_scope_cancel();
                assert_eq!(
                    canceled
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop())),
                    Poll::Ready(Err(crate::Errno::CANCELED))
                );
                *pending.borrow_mut() = Some(operations::nop());
            })
        };
        futures::future::poll_fn(|cx| {
            let waker = reentrant_waker(cx.waker(), calls.clone(), Some(on_wake.clone()));
            assert!(
                wait.as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            Poll::Ready(())
        })
        .await;
        operations::io_scope_cancel();
        assert_eq!(wait.await, Err(CanceledError {}));
        let registrations = task
            .io_scope_completions
            .use_mut(|scope| scope.as_ref().unwrap().test_completions());
        assert_eq!(registrations.len(), 1);
        registrations[0]
            .state
            .use_mut(|state| assert!(matches!(state, crate::CompletionState::Idle { .. })));
        let nop = pending.borrow_mut().take().unwrap();
        nop.await.unwrap();
    })
    .await;
}

#[crate::test]
async fn cancel_snapshot_precedes_original_cqe_wake_callbacks() {
    operations::io_scope(async || {
        let task = TaskState::get().get_current_task();
        let pending = Rc::new(RefCell::new(None));
        let calls = Rc::new(Cell::new(0));
        let on_wake: Rc<dyn Fn()> = {
            let pending = pending.clone();
            Rc::new(move || *pending.borrow_mut() = Some(operations::nop()))
        };
        let mut original = Box::pin(operations::nop());
        futures::future::poll_fn(|cx| {
            let waker = reentrant_waker(cx.waker(), calls.clone(), Some(on_wake.clone()));
            assert!(
                original
                    .as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            Poll::Ready(())
        })
        .await;
        // Queue the true CQE without delivering its wake callback yet.
        TaskState::get().ring.submit_and_wait(1);
        operations::io_scope_cancel();
        let registrations = task
            .io_scope_completions
            .use_mut(|scope| scope.as_ref().unwrap().test_completions());
        assert_eq!(registrations.len(), 1);
        registrations[0]
            .state
            .use_mut(|state| assert!(matches!(state, crate::CompletionState::Idle { .. })));
        original.await.unwrap();
        let fresh = pending.borrow_mut().take().unwrap();
        fresh.await.unwrap();
    })
    .await;
}

#[crate::test]
async fn original_cqe_retires_even_when_result_future_is_retained() {
    operations::io_scope(async || {
        let task = TaskState::get().get_current_task();
        let mut nop = Box::pin(operations::nop());
        assert!(futures::poll!(nop.as_mut()).is_pending());
        let original = task
            .io_scope_completions
            .use_mut(|scope| scope.as_ref().unwrap().test_completions().pop().unwrap());
        for _ in 0..16384 {
            drop(crate::runtime::submit_and_complete_io_all(
                TaskState::get(),
                true,
            ));
            if original
                .state
                .use_mut(|state| matches!(state, crate::CompletionState::Completed { .. }))
            {
                break;
            }
        }
        original.state.use_mut(|state| {
            assert!(matches!(
                state,
                crate::CompletionState::Completed { result: Ok(0), .. }
            ))
        });
        task.io_scope_completions
            .use_mut(|scope| assert!(scope.as_ref().unwrap().is_empty()));
        assert_eq!(futures::poll!(nop.as_mut()), Poll::Ready(Ok(())));
    })
    .await;
}
