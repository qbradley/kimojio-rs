// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
use super::*;
use crate::{AsyncEvent, CanceledError, CompletionResources, io_type::IOType, operations};
use futures::{StreamExt, stream::FuturesUnordered};
use std::{
    cell::RefCell,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

#[crate::test]
async fn wrapped_wait_explicit_cancellation() {
    let event = AsyncEvent::new();
    operations::io_scope(async || {
        let mut waits = FuturesUnordered::new();
        waits.push(event.wait());
        assert!(futures::poll!(waits.next()).is_pending());
        operations::io_scope_cancel();
        assert_eq!(waits.next().await, Some(Err(CanceledError {})));
        assert_eq!(waits.next().await, None);
    })
    .await;
}

#[crate::test]
async fn wrapped_wait_scope_exit() {
    let event = AsyncEvent::new();
    let mut waits = FuturesUnordered::new();
    operations::io_scope(async || {
        waits.push(event.wait());
        assert!(futures::poll!(waits.next()).is_pending());
    })
    .await;
    assert_eq!(waits.next().await, Some(Err(CanceledError {})));
    assert_eq!(waits.next().await, None);
}

#[crate::test]
async fn wrapped_wait_scope_drop() {
    let event = AsyncEvent::new();
    let mut waits = FuturesUnordered::new();
    let mut scope = Box::pin(operations::io_scope(async || {
        waits.push(event.wait());
        assert!(futures::poll!(waits.next()).is_pending());
        futures::future::pending::<()>().await;
    }));
    assert!(futures::poll!(scope.as_mut()).is_pending());
    drop(scope);
    assert_eq!(waits.next().await, Some(Err(CanceledError {})));
    assert_eq!(waits.next().await, None);
}

#[crate::test]
async fn wrapped_wait_normal_notification() {
    let event = AsyncEvent::new();
    operations::io_scope(async || {
        let mut waits = FuturesUnordered::new();
        waits.push(event.wait());
        assert!(futures::poll!(waits.next()).is_pending());
        event.set();
        assert_eq!(waits.next().await, Some(Ok(())));
        assert_eq!(waits.next().await, None);
    })
    .await;
}

#[crate::test]
async fn wrapped_sibling_connections_are_not_canceled() {
    use futures::FutureExt;
    let (canceled_fd, _canceled_peer) = crate::pipe::bipipe();
    let (sibling_fd, sibling_peer) = crate::pipe::bipipe();
    let cancel = AsyncEvent::new();
    let mut connections = FuturesUnordered::new();
    connections.push(
        operations::io_scope(async || {
            let mut buffer = [0; 1];
            let mut read = operations::read(&canceled_fd, &mut buffer);
            assert!(futures::poll!(&mut read).is_pending());
            cancel.wait().await.unwrap();
            operations::io_scope_cancel();
            assert_eq!(read.await, Err(crate::Errno::CANCELED));
            0
        })
        .boxed_local(),
    );
    connections.push(
        operations::io_scope(async || {
            let mut buffer = [0; 1];
            assert_eq!(operations::read(&sibling_fd, &mut buffer).await, Ok(1));
            assert_eq!(buffer, *b"s");
            1
        })
        .boxed_local(),
    );
    assert!(futures::poll!(connections.next()).is_pending());
    cancel.set();
    assert_eq!(connections.next().await, Some(0));
    assert!(futures::poll!(connections.next()).is_pending());
    assert_eq!(operations::write(&sibling_peer, b"s").await, Ok(1));
    assert_eq!(connections.next().await, Some(1));
    assert_eq!(connections.next().await, None);
}

#[crate::test]
async fn native_waker_can_drop_completed_task_result() {
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
        let task = Task::new(
            std::future::ready(Probe(drops.clone())),
            0,
            state_ptr,
            Default::default(),
            Default::default(),
        );
        assert!(
            task.poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
        task.set_state(TaskReadyState::Complete);
        let waker = crate::task_ref::create_waker(task);
        if direct {
            waker.wake();
        } else {
            drop(wake_task(TaskState::get(), waker));
        }
    }
    assert_eq!(drops.get(), 2);
}

#[crate::test]
async fn completed_self_woken_task_queue_owner_drops_outside_task_state() {
    struct Probe(Rc<Cell<bool>>);
    impl Drop for Probe {
        fn drop(&mut self) {
            drop(TaskState::get());
            self.0.set(true);
        }
    }
    let dropped = Rc::new(Cell::new(false));
    let probe = Probe(dropped.clone());
    drop(operations::spawn_task(async move {
        futures::future::poll_fn(|cx| {
            cx.waker().wake_by_ref();
            Poll::Ready(())
        })
        .await;
        probe
    }));
    for _ in 0..10 {
        operations::yield_io().await;
        if dropped.get() {
            return;
        }
    }
    panic!("completed task result was not released");
}

// Single-threaded raw wakers deliberately reenter both TaskState and the
// current Vec registry on clone, replacement/drop, and wake callbacks.
struct ReentrantWaker {
    native: Waker,
    calls: Rc<Cell<usize>>,
    on_wake: Option<Rc<dyn Fn()>>,
}

fn touch(probe: &ReentrantWaker) {
    let state = TaskState::get();
    let task = state.current_task.clone();
    drop(state);
    if let Some(task) = task {
        let waits = task.io_scope_completions.use_mut(|scope| {
            scope
                .as_ref()
                .map(|scope| scope.waits.clone())
                .unwrap_or_default()
        });
        for wait in waits {
            wait.waker.use_mut(|_| ());
        }
        drop(operations::nop());
    }
    probe.calls.set(probe.calls.get() + 1);
}

const REENTRANT_VTABLE: RawWakerVTable = RawWakerVTable::new(
    |data| {
        // SAFETY: clone borrows an existing Rc-backed raw-waker owner.
        touch(unsafe { &*data.cast::<ReentrantWaker>() });
        unsafe { Rc::increment_strong_count(data.cast::<ReentrantWaker>()) };
        RawWaker::new(data, &REENTRANT_VTABLE)
    },
    |data| {
        // SAFETY: wake consumes exactly this raw-waker's Rc owner.
        let probe = unsafe { Rc::from_raw(data.cast::<ReentrantWaker>()) };
        touch(&probe);
        if let Some(on_wake) = &probe.on_wake {
            on_wake();
        }
        probe.native.wake_by_ref();
    },
    |data| {
        // SAFETY: wake_by_ref only borrows the raw-waker owner.
        let probe = unsafe { &*data.cast::<ReentrantWaker>() };
        touch(probe);
        if let Some(on_wake) = &probe.on_wake {
            on_wake();
        }
        probe.native.wake_by_ref();
    },
    |data| {
        // SAFETY: drop consumes exactly this raw-waker's Rc owner.
        let probe = unsafe { Rc::from_raw(data.cast::<ReentrantWaker>()) };
        touch(&probe);
    },
);

fn reentrant_waker(native: &Waker, calls: Rc<Cell<usize>>, on_wake: Option<Rc<dyn Fn()>>) -> Waker {
    let probe = Rc::new(ReentrantWaker {
        native: native.clone(),
        calls,
        on_wake,
    });
    // SAFETY: the vtable maintains one Rc reference per raw-waker owner.
    unsafe { Waker::from_raw(RawWaker::new(Rc::into_raw(probe).cast(), &REENTRANT_VTABLE)) }
}

#[crate::test]
async fn wait_and_io_waker_callbacks_can_reenter() {
    operations::io_scope(async || {
        let event = AsyncEvent::new();
        let calls = Rc::new(Cell::new(0));
        futures::future::poll_fn(|cx| {
            let waker = reentrant_waker(cx.waker(), calls.clone(), None);
            let mut cx = Context::from_waker(&waker);
            let mut wait = Box::pin(event.wait());
            assert!(wait.as_mut().poll(&mut cx).is_pending());
            assert!(wait.as_mut().poll(&mut cx).is_pending());
            event.set();
            event.reset();
            operations::io_scope_cancel();
            assert_eq!(
                wait.as_mut().poll(&mut cx),
                Poll::Ready(Err(CanceledError {}))
            );
            Poll::Ready(())
        })
        .await;
        assert!(calls.get() >= 4);

        let mut nop = Box::pin(operations::nop());
        let mut first = true;
        futures::future::poll_fn(|cx| {
            let waker = reentrant_waker(cx.waker(), calls.clone(), None);
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
    })
    .await;
}

#[crate::test]
async fn cancellation_callbacks_preserve_new_vec_registrations() {
    operations::io_scope(async || {
        let task = TaskState::get().get_current_task();
        let calls = Rc::new(Cell::new(0));
        let pending = Rc::new(RefCell::new(None));
        let on_wake: Rc<dyn Fn()> = {
            let pending = pending.clone();
            Rc::new(move || *pending.borrow_mut() = Some(operations::nop()))
        };
        let event = AsyncEvent::new();
        let mut wait = Box::pin(event.wait());
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
        // The old batch was drained, but wake/drop callbacks created new NOPs
        // in the still-installed registry. Both live work and dropped-operation
        // history must survive this pass.
        let registrations = task.io_scope_completions.use_mut(|scope| {
            let scope = scope.as_ref().unwrap();
            assert!(scope.waits.is_empty());
            scope.completions.clone()
        });
        assert!(!registrations.is_empty());
        assert_eq!(
            registrations
                .iter()
                .filter(|completion| completion
                    .state
                    .use_mut(|state| { matches!(state, crate::CompletionState::Idle { .. }) }))
                .count(),
            1,
            "the live callback NOP was not canceled by the old pass"
        );
        assert!(
            registrations.iter().any(|completion| completion
                .state
                .use_mut(|state| { matches!(state, crate::CompletionState::Terminated) })),
            "dropped callback NOPs remain in the Vec history"
        );
        assert_eq!(wait.await, Err(CanceledError {}));
        operations::io_scope_cancel();
        let pending_nop = pending.borrow_mut().take().unwrap();
        assert_eq!(pending_nop.await, Err(crate::Errno::CANCELED));
        task.io_scope_completions.use_mut(|scope| {
            assert!(scope.as_ref().unwrap().completions.is_empty());
        });
    })
    .await;
}

#[crate::test]
async fn cancellation_callback_can_cancel_recursively() {
    operations::io_scope(async || {
        let event = AsyncEvent::new();
        let mut wait = Box::pin(event.wait());
        let calls = Rc::new(Cell::new(0));
        futures::future::poll_fn(|cx| {
            let waker = reentrant_waker(
                cx.waker(),
                calls.clone(),
                Some(Rc::new(operations::io_scope_cancel)),
            );
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
    })
    .await;
}

struct ResourceProbe(Rc<Cell<usize>>);
impl Drop for ResourceProbe {
    fn drop(&mut self) {
        let active = TaskState::get().current_task.is_some();
        if active {
            drop(operations::nop());
        }
        self.0.set(self.0.get() + 1);
    }
}

fn resource_nop(drops: Rc<Cell<usize>>) -> crate::ring_future::UnitFuture<'static> {
    crate::ring_future::UnitFuture::with_polled(
        rustix_uring::opcode::Nop::new().build(),
        0,
        None,
        IOType::Nop,
        false,
        CompletionResources::Box(Box::new(ResourceProbe(drops))),
    )
}

#[crate::test]
async fn completion_pool_reuse_drops_resources_outside_borrows() {
    let old_pool = std::mem::take(&mut TaskState::get().completion_pool);
    drop(old_pool);
    let drops = Rc::new(Cell::new(0));
    operations::io_scope(async || {
        resource_nop(drops.clone()).await.unwrap();
        assert_eq!(drops.get(), 0, "Vec history still owns the resource");
    })
    .await;
    assert_eq!(drops.get(), 0, "scope cleanup pooled the unique completion");
    operations::io_scope(async || {
        let nop = operations::nop();
        assert_eq!(drops.get(), 1, "pool reuse destroys old resources");
        // The destructor's NOP was registered without recursively borrowing
        // TaskState and was not overwritten by the outer registration.
        let task = TaskState::get().get_current_task();
        task.io_scope_completions.use_mut(|scope| {
            assert_eq!(scope.as_ref().unwrap().completions.len(), 2);
        });
        nop.await.unwrap();
    })
    .await;
}

fn fill_completion_pool() {
    let mut state = TaskState::get();
    while state.completion_pool.len() < MAX_COMPLETION_POOL_ENTRIES {
        state.completion_pool.push_back(Rc::new(Completion {
            state: MutInPlaceCell::new(crate::CompletionState::Terminated),
            owned_resources: CompletionResources::None,
            timespec: Default::default(),
            tag: 0,
            task_index: 0,
            iopoll: false,
        }));
    }
}

#[crate::test]
async fn unpooled_resources_drop_outside_task_state() {
    let drops = Rc::new(Cell::new(0));
    // A full pool rejects the final future owner when its result is consumed.
    let mut nop = Box::pin(resource_nop(drops.clone()));
    assert!(futures::poll!(nop.as_mut()).is_pending());
    fill_completion_pool();
    // Drain synchronously so other tasks cannot consume a pool slot before
    // this future releases its final owner.
    loop {
        drop(crate::runtime::submit_and_complete_io_all(
            TaskState::get(),
            true,
        ));
        if let Poll::Ready(result) = futures::poll!(nop.as_mut()) {
            result.unwrap();
            break;
        }
    }
    assert_eq!(drops.get(), 1);

    // Also cover a CQE releasing the last owner of abandoned owned-resource I/O.
    let mut nop = Box::pin(resource_nop(drops.clone()));
    assert!(futures::poll!(nop.as_mut()).is_pending());
    drop(nop);
    fill_completion_pool();
    while drops.get() != 2 {
        drop(crate::runtime::submit_and_complete_io_all(
            TaskState::get(),
            true,
        ));
    }
    assert_eq!(drops.get(), 2);
}
