// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! Bounded diagnostics on x86_64, Rust 1.98.1, compared this candidate with a4af94fd.
//! Before these extra cases, 31 scope/wait tests retired 27 zero-member,
//! 4,623 one-member, and one two-member generations on each implementation.
//! Default/all-feature, debug/release runs gave identical counts.
//! The default-debug HTTP2 test
//! `generic_and_native_roles_share_concurrent_duplex_connect_and_observers`
//! retired 609 zero-member and 16,786 one-member generations on each implementation.
//! No multi-member case occurred.
//! These instrumented tests do not establish a production workload distribution or a speed change.
//!
//! Both implementations measured WaitData at 96 bytes in debug and 80 in release.
//! Completion measured 144/128 bytes, or 208/192 with io_uring_cmd.
//! The inline enum and the old vector each measured 24 bytes with alignment 8.
//! Thus the requested Rc allocation layout does not increase on this target.
//! The final runtime code excludes the diagnostic hooks. The layout test remains.

use super::*;
use crate::{AsyncEvent, CanceledError, operations};
use futures::future::join_all;
use std::{cell::RefCell, task::Poll};

#[test]
fn membership_layout_matches_vector_baseline() {
    assert_eq!(
        std::mem::size_of::<WaitScopes>(),
        std::mem::size_of::<Vec<Weak<IoScopeRegistry>>>(),
    );
    assert_eq!(
        std::mem::align_of::<WaitScopes>(),
        std::mem::align_of::<Vec<Weak<IoScopeRegistry>>>(),
    );
    #[cfg(target_arch = "x86_64")]
    {
        assert_eq!(std::mem::size_of::<WaitScopes>(), 24);
        assert_eq!(std::mem::align_of::<WaitScopes>(), 8);
        assert_eq!(
            std::mem::size_of::<WaitData>(),
            if cfg!(debug_assertions) { 96 } else { 80 },
        );
        let completion_size = if cfg!(debug_assertions) { 144 } else { 128 };
        assert_eq!(
            std::mem::size_of::<Completion>(),
            completion_size
                + if cfg!(feature = "io_uring_cmd") {
                    64
                } else {
                    0
                },
        );
    }
    eprintln!(
        "LAYOUT debug={} WaitData={} Completion={} Vec={} Inline={}",
        cfg!(debug_assertions),
        std::mem::size_of::<WaitData>(),
        std::mem::size_of::<Completion>(),
        std::mem::size_of::<Vec<std::rc::Weak<IoScopeRegistry>>>(),
        std::mem::size_of::<WaitScopes>(),
    );
}

#[test]
fn empty_inline_and_fallback_memberships_keep_only_weak_owners() {
    let mut memberships = WaitScopes::default();
    assert!(matches!(memberships, WaitScopes::Empty));
    let first = Rc::new(IoScopeRegistry::default());
    memberships.push(Rc::downgrade(&first));
    assert_eq!(Rc::strong_count(&first), 1);
    assert_eq!(Rc::weak_count(&first), 1);
    assert!(matches!(memberships, WaitScopes::One(_)));
    let second = Rc::new(IoScopeRegistry::default());
    memberships.push(Rc::downgrade(&second));
    assert_eq!(Rc::strong_count(&first), 1);
    assert_eq!(Rc::weak_count(&first), 1);
    assert_eq!(Rc::strong_count(&second), 1);
    assert!(matches!(&memberships, WaitScopes::Many(scopes) if scopes.len() == 2));
    drop(first);
    // An expired member does not prevent retirement through another live member.
    memberships.retire(std::ptr::null());
    assert_eq!(Rc::weak_count(&second), 0);
    let mut one = WaitScopes::default();
    one.push(Rc::downgrade(&second));
    drop(second);
    one.retire(std::ptr::null());
    WaitScopes::default().retire(std::ptr::null());
}

#[crate::test]
async fn real_waiter_memberships_grow_deduplicate_and_retire_all_scopes() {
    for finish in 0..3 {
        let event = AsyncEvent::new();
        let wait = RefCell::new(Some(Box::pin(event.wait())));
        let registries = RefCell::new(Vec::new());
        let mut scopes = Vec::new();
        for index in 0..16 {
            let mut scope = Box::pin(operations::io_scope(async || {
                registries.borrow_mut().push(current_registry());
                futures::future::poll_fn(|cx| {
                    assert!(
                        wait.borrow_mut()
                            .as_mut()
                            .unwrap()
                            .as_mut()
                            .poll(cx)
                            .is_pending()
                    );
                    Poll::<()>::Pending
                })
                .await
            }));
            for _ in 0..4 {
                assert!(futures::poll!(scope.as_mut()).is_pending());
            }
            let registered = registries.borrow()[0]
                .entries
                .use_mut(|entries| entries.waits.values().next().unwrap().clone());
            registered.scopes.use_mut(|memberships| match memberships {
                WaitScopes::One(_) => assert_eq!(index, 0),
                WaitScopes::Many(memberships) => assert_eq!(memberships.len(), index + 1),
                WaitScopes::Empty => panic!("pending waiter lost its memberships"),
            });
            scopes.push(scope);
        }
        let registered = registries.borrow()[0]
            .entries
            .use_mut(|entries| entries.waits.values().next().unwrap().clone());
        assert!(
            registries
                .borrow()
                .iter()
                .all(|registry| counts(registry).2 == 1)
        );
        match finish {
            0 => {
                event.set();
                futures::future::poll_fn(|cx| {
                    wait.borrow_mut().as_mut().unwrap().as_mut().poll(cx)
                })
                .await
                .unwrap();
            }
            1 => drop(wait.borrow_mut().take()),
            2 => {
                drop(scopes.pop().unwrap());
                assert_eq!(
                    futures::future::poll_fn(|cx| {
                        wait.borrow_mut().as_mut().unwrap().as_mut().poll(cx)
                    })
                    .await,
                    Err(CanceledError {}),
                );
                operations::io_scope(async || {
                    let fresh = current_registry();
                    futures::future::poll_fn(|cx| {
                        assert!(
                            wait.borrow_mut()
                                .as_mut()
                                .unwrap()
                                .as_mut()
                                .poll(cx)
                                .is_pending()
                        );
                        Poll::Ready(())
                    })
                    .await;
                    let new = fresh
                        .entries
                        .use_mut(|entries| entries.waits.values().next().unwrap().clone());
                    assert!(!Rc::ptr_eq(&registered, &new));
                    assert!(
                        new.scopes
                            .use_mut(|memberships| matches!(memberships, WaitScopes::One(_)))
                    );
                    scopes.clear();
                    event.set();
                    futures::future::poll_fn(|cx| {
                        wait.borrow_mut().as_mut().unwrap().as_mut().poll(cx)
                    })
                    .await
                    .unwrap();
                    assert!(fresh.is_empty());
                })
                .await;
            }
            _ => unreachable!(),
        }
        assert!(
            registered
                .scopes
                .use_mut(|memberships| matches!(memberships, WaitScopes::Empty))
        );
        assert!(
            registries
                .borrow()
                .iter()
                .all(|registry| registry.is_empty())
        );
        drop(scopes);
    }
}

#[crate::test]
async fn waiter_migrates_across_tasks_and_foreign_wakers_without_losing_scopes() {
    use futures::{StreamExt, stream::FuturesUnordered};

    let finish = Rc::new(AsyncEvent::new());
    let subject = operations::spawn_task({
        let finish = finish.clone();
        async move { finish.wait().await.unwrap() }
    });
    let wait = Rc::new(RefCell::new(Box::pin(subject.wait())));
    let first_ready = Rc::new(AsyncEvent::new());
    let release_first = Rc::new(AsyncEvent::new());
    let first_capture = Rc::new(RefCell::new(None));
    let first = operations::spawn_task({
        let wait = wait.clone();
        let first_ready = first_ready.clone();
        let release_first = release_first.clone();
        let first_capture = first_capture.clone();
        async move {
            operations::io_scope(async || {
                let registry = current_registry();
                futures::future::poll_fn(|cx| {
                    assert!(wait.borrow_mut().as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                let registered = registry
                    .entries
                    .use_mut(|entries| entries.waits.values().next().unwrap().clone());
                *first_capture.borrow_mut() = Some((registry, registered));
                first_ready.set();
                release_first.wait().await.unwrap();
            })
            .await;
        }
    });
    first_ready.wait().await.unwrap();
    let (first_registry, registered) = first_capture.borrow_mut().take().unwrap();
    let second_ready = Rc::new(AsyncEvent::new());
    let second = operations::spawn_task({
        let wait = wait.clone();
        let registered = registered.clone();
        let second_ready = second_ready.clone();
        async move {
            operations::io_scope(async || {
                let registry = current_registry();
                let mut pending = FuturesUnordered::new();
                pending.push(futures::future::poll_fn(|cx| {
                    wait.borrow_mut().as_mut().poll(cx)
                }));
                let mut next = Box::pin(pending.next());
                assert!(futures::poll!(next.as_mut()).is_pending());
                assert!(registered.scopes.use_mut(|memberships| {
                    matches!(memberships, WaitScopes::Many(scopes) if scopes.len() == 2)
                }));
                second_ready.set();
                assert_eq!(next.await, Some(Ok(())));
                assert!(registry.is_empty());
            })
            .await;
        }
    });
    second_ready.wait().await.unwrap();
    finish.set();
    second.await.unwrap();
    assert!(!first.is_complete());
    assert!(
        registered
            .scopes
            .use_mut(|memberships| matches!(memberships, WaitScopes::Empty))
    );
    assert!(
        !first_registry
            .entries
            .use_mut(|entries| entries.waits.contains_key(&Rc::as_ptr(&registered)))
    );
    release_first.set();
    first.await.unwrap();
    assert!(first_registry.is_empty());
    subject.await.unwrap();
}

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
            for multiple in [false, true] {
                let other = Rc::new(IoScopeRegistry::default());
                let mut wait = Box::pin(event.wait());
                assert!(wait.as_mut().poll(&mut cx).is_pending());
                assert!(wait.as_mut().poll(&mut cx).is_pending());
                if multiple {
                    let registered = registry
                        .entries
                        .use_mut(|entries| entries.waits.values().next().unwrap().clone());
                    other.register_wait(&registered);
                }
                drop(wait);
                assert!(registry.is_empty());
                assert!(other.is_empty());
                let mut wait = Box::pin(event.wait());
                assert!(wait.as_mut().poll(&mut cx).is_pending());
                if multiple {
                    let registered = registry
                        .entries
                        .use_mut(|entries| entries.waits.values().next().unwrap().clone());
                    other.register_wait(&registered);
                }
                operations::io_scope_cancel();
                assert_eq!(
                    wait.as_mut().poll(&mut cx),
                    Poll::Ready(Err(CanceledError {}))
                );
                assert!(registry.is_empty());
                assert!(other.is_empty());
            }
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
