// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
use super::*;
use crate::{AsyncEvent, CanceledError, operations};
use futures::future::join_all;
use std::{cell::RefCell, task::Poll};

fn current_registry() -> Rc<IoScopeRegistry> {
    TaskState::get()
        .get_current_task()
        .scope_registry()
        .unwrap()
}

fn counts(registry: &IoScopeRegistry) -> (usize, usize, usize, usize) {
    registry.entries.use_mut(|entries| {
        (
            entries.completions.len(),
            entries.completions.capacity(),
            entries.waits.len(),
            entries.waits.capacity(),
        )
    })
}

#[crate::test]
async fn scope_native_waker_can_drop_a_completed_task_result_reentrantly() {
    use std::{cell::Cell, task::Context};

    struct Probe(Rc<Cell<usize>>);
    impl Drop for Probe {
        fn drop(&mut self) {
            drop(TaskState::get());
            self.0.set(self.0.get() + 1);
        }
    }

    let drops = Rc::new(Cell::new(0));
    for direct in [false, true] {
        let state = TaskState::get();
        let state_ptr = &*state as *const TaskState;
        drop(state);
        let task = super::super::Task::new(
            std::future::ready(Probe(drops.clone())),
            0,
            state_ptr,
            Default::default(),
            Default::default(),
        );
        assert!(
            task.poll(&mut Context::from_waker(std::task::Waker::noop()))
                .is_ready()
        );
        task.set_state(super::super::TaskReadyState::Complete);
        let waker = crate::task_ref::create_waker(task);
        if direct {
            waker.wake();
        } else {
            drop(crate::task_ref::wake_task(TaskState::get(), waker));
        }
    }
    assert_eq!(drops.get(), 2);
}

#[crate::test]
async fn long_scope_retires_completed_io_and_shrinks_concurrency_slack() {
    operations::io_scope(async || {
        let registry = current_registry();
        for _ in 0..4096 {
            operations::nop().await.unwrap();
            assert_eq!(counts(&registry).0, 0);
        }
        let nops: Vec<_> = (0..512).map(|_| operations::nop()).collect();
        assert_eq!(counts(&registry).0, 512);
        for result in join_all(nops).await {
            result.unwrap();
        }
        assert_eq!(counts(&registry).0, 0);
        assert!(counts(&registry).1 <= 64);
        let mut unobserved = Box::pin(operations::nop());
        assert!(futures::poll!(unobserved.as_mut()).is_pending());
        assert_eq!(counts(&registry).0, 1);
        while counts(&registry).0 != 0 {
            operations::yield_io().await;
        }
        // The CQE retires the entry before the caller consumes its result.
        unobserved.await.unwrap();
        // Idle cancellation also retires registrations without a kernel CQE.
        for _ in 0..4096 {
            drop(operations::nop());
            assert_eq!(counts(&registry).0, 0);
        }
        assert!(counts(&registry).1 <= 64);
    })
    .await;
}

#[crate::test]
async fn long_scope_retires_ready_and_dropped_wait_generations() {
    operations::io_scope(async || {
        let registry = current_registry();
        let event = AsyncEvent::new();
        for index in 0..4096 {
            event.reset();
            let mut wait = Box::pin(event.wait());
            assert!(futures::poll!(wait.as_mut()).is_pending());
            assert_eq!(counts(&registry).2, 1);
            if index % 2 == 0 {
                event.set();
                assert_eq!(futures::poll!(wait.as_mut()), Poll::Ready(Ok(())));
                assert_eq!(
                    counts(&registry).2,
                    0,
                    "ready future still owns a registration"
                );
            }
            drop(wait);
            assert_eq!(counts(&registry).2, 0);
        }
        let mut waits: Vec<_> = (0..512).map(|_| Box::pin(event.wait_reset())).collect();
        event.set();
        for wait in &mut waits {
            assert!(futures::poll!(wait.as_mut()).is_pending());
        }
        assert_eq!(counts(&registry).2, 512);
        drop(waits);
        assert_eq!(counts(&registry).2, 0);
        assert!(counts(&registry).3 <= 64);
    })
    .await;
}

#[crate::test]
async fn repeated_pending_polls_deduplicate_without_forgetting_woken_waits() {
    operations::io_scope(async || {
        let registry = current_registry();
        let event = AsyncEvent::new();
        let mut wait = Box::pin(event.wait());
        for _ in 0..4096 {
            assert!(futures::poll!(wait.as_mut()).is_pending());
            assert_eq!(counts(&registry).2, 1);
        }
        event.set();
        assert!(!event.any_waiting());
        assert_eq!(counts(&registry).2, 1, "wake is not wait completion");
        operations::io_scope_cancel();
        assert_eq!(wait.as_mut().await, Err(CanceledError {}));
        assert_eq!(counts(&registry).2, 0);
        event.reset();
        assert!(futures::poll!(wait.as_mut()).is_pending());
        assert_eq!(counts(&registry).2, 1);
        event.set();
        wait.await.unwrap();
        assert_eq!(counts(&registry).2, 0);
    })
    .await;
}

#[crate::test]
async fn old_scope_cancellation_does_not_reach_a_retried_wait_generation() {
    let event = AsyncEvent::new();
    let wait = Rc::new(RefCell::new(Box::pin(event.wait())));
    let pending = || {
        futures::future::poll_fn(|cx| {
            assert!(wait.borrow_mut().as_mut().poll(cx).is_pending());
            Poll::<()>::Pending
        })
    };
    let mut first = Box::pin(operations::io_scope(async || pending().await));
    let mut second = Box::pin(operations::io_scope(async || pending().await));
    assert!(futures::poll!(first.as_mut()).is_pending());
    assert!(futures::poll!(second.as_mut()).is_pending());
    drop(first);
    assert_eq!(
        futures::future::poll_fn(|cx| wait.borrow_mut().as_mut().poll(cx)).await,
        Err(CanceledError {}),
    );
    // Retry outside either scope. The second scope only saw the old generation.
    assert!(
        futures::future::poll_fn(|cx| {
            Poll::Ready(wait.borrow_mut().as_mut().poll(cx).is_pending())
        })
        .await
    );
    drop(second);
    event.set();
    futures::future::poll_fn(|cx| wait.borrow_mut().as_mut().poll(cx))
        .await
        .unwrap();
}

#[crate::test]
async fn scope_wait_and_io_wakers_can_reenter_task_state_and_registry() {
    use std::{
        cell::Cell,
        task::{Context, RawWaker, RawWakerVTable, Waker},
    };

    struct Probe {
        native: Waker,
        drops: Rc<Cell<usize>>,
    }
    fn touch(probe: &Probe) {
        let task_state = TaskState::get();
        let active = task_state.current_task.is_some();
        drop(task_state);
        if active {
            drop(operations::nop());
        }
        probe.drops.set(probe.drops.get() + 1);
    }
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |data| {
            // SAFETY: clone borrows an existing raw-waker owner.
            touch(unsafe { &*data.cast::<Probe>() });
            // SAFETY: every raw-waker owner contributes one strong reference.
            unsafe { Rc::increment_strong_count(data.cast::<Probe>()) };
            RawWaker::new(data, &VTABLE)
        },
        |data| {
            // SAFETY: wake consumes this raw-waker owner exactly once.
            let probe = unsafe { Rc::from_raw(data.cast::<Probe>()) };
            touch(&probe);
            probe.native.wake_by_ref();
        },
        |data| {
            // SAFETY: wake_by_ref borrows an existing raw-waker owner.
            let probe = unsafe { &*data.cast::<Probe>() };
            touch(probe);
            probe.native.wake_by_ref();
        },
        |data| {
            // SAFETY: drop consumes this raw-waker owner exactly once.
            let probe = unsafe { Rc::from_raw(data.cast::<Probe>()) };
            touch(&probe);
        },
    );

    operations::io_scope(async || {
        let registry = current_registry();
        let event = AsyncEvent::new();
        let drops = Rc::new(Cell::new(0));
        futures::future::poll_fn(|cx| {
            let probe = Rc::new(Probe {
                native: cx.waker().clone(),
                drops: drops.clone(),
            });
            // SAFETY: the vtable preserves and consumes Rc owners as documented.
            let waker =
                unsafe { Waker::from_raw(RawWaker::new(Rc::into_raw(probe).cast(), &VTABLE)) };
            let mut cx = Context::from_waker(&waker);
            let mut wait = Box::pin(event.wait());
            assert!(wait.as_mut().poll(&mut cx).is_pending());
            assert!(wait.as_mut().poll(&mut cx).is_pending());
            drop(wait);
            assert!(registry.is_empty());
            let mut wait = Box::pin(event.wait());
            assert!(wait.as_mut().poll(&mut cx).is_pending());
            operations::io_scope_cancel();
            assert_eq!(
                wait.as_mut().poll(&mut cx),
                Poll::Ready(Err(CanceledError {}))
            );
            assert!(registry.is_empty());
            Poll::Ready(())
        })
        .await;
        assert!(drops.get() >= 3);
        assert!(registry.is_empty());
        let mut nop = Box::pin(operations::nop());
        let mut first = true;
        futures::future::poll_fn(|cx| {
            let probe = Rc::new(Probe {
                native: cx.waker().clone(),
                drops: drops.clone(),
            });
            // SAFETY: the same single-threaded Rc owner protocol applies.
            let waker =
                unsafe { Waker::from_raw(RawWaker::new(Rc::into_raw(probe).cast(), &VTABLE)) };
            let mut cx = Context::from_waker(&waker);
            let result = nop.as_mut().poll(&mut cx);
            if first {
                first = false;
                assert!(result.is_pending());
                assert!(nop.as_mut().poll(&mut cx).is_pending());
            }
            result
        })
        .await
        .unwrap();
        assert!(registry.is_empty());
    })
    .await;
}
