// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! RingFuture implements a Future that is completed
//! via I/O URing .
//!
//! It is generic on the return type via the `MakeResult` trait
//! and a closure that creates the `Entry` which corresponds to
//! the I/O URing SQE.
//!
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Duration;

use rustix::fd::FromRawFd;
use rustix::io_uring::io_uring_user_data;
use rustix_uring::opcode;
use rustix_uring::squeue::Flags;
use rustix_uring::types::Timespec;

use crate::io_type::IOType;
use crate::operations::IsIoPoll;
use crate::runtime::submit_and_complete_io;
use crate::tracing::Events;
use crate::{CompletionResources, CompletionState, Errno, MutInPlaceCell};
pub use rustix::fd::OwnedFd;

use crate::Completion;
use crate::task::{TaskReadyState, TaskState};

#[cfg(feature = "io_uring_cmd")]
use rustix_uring::squeue::Entry128 as SQE;

#[cfg(not(feature = "io_uring_cmd"))]
use rustix_uring::squeue::Entry as SQE;

// Future representing operations which return either 0 or an error value.
// Underlying result type is Result<()> which will contain () or the error
// if the operation failed
pub type UnitFuture<'a> = RingFuture<'a, (), ResultToUnit>;

// Future for operations which return zero or positive on success,
// and a negative value on error.
pub type UsizeFuture<'a> = RingFuture<'a, usize, ResultToUsize>;

// Future for operations which return a zero or positive file descriptor
// on success and negative value on error.
pub type OwnedFdFuture<'a> = RingFuture<'a, OwnedFd, ResultToOwnedFd>;

#[cfg(feature = "io_uring_cmd")]
pub type UringCmdFuture<'a> = RingFuture<'a, [u64; 2], ResultToCqe>;

pub trait MakeResult<T: Unpin>: Unpin {
    fn make_success(value: u32, cqe: &[u64; 2]) -> T;
}

pub struct RingFuture<'a, T: Unpin, C: MakeResult<T>> {
    handle: Option<Rc<Completion>>,
    _marker: std::marker::PhantomData<(&'a (), T, C)>,
}

impl<'a, T: Unpin, C: MakeResult<T>> RingFuture<'a, T, C> {
    pub(crate) fn new<Entry: Into<SQE>>(
        entry: Entry,
        fd: i32,
        timeout: Option<Duration>,
        io_type: IOType,
    ) -> Self {
        Self::with_polled(
            entry.into(),
            fd,
            timeout,
            io_type,
            false,
            CompletionResources::None,
        )
    }

    pub(crate) fn with_polled<Entry: Into<SQE>>(
        entry: Entry,
        fd: i32,
        timeout: Option<Duration>,
        io_type: IOType,
        iopoll: bool,
        owned_resources: CompletionResources,
    ) -> Self {
        let entry = Some(entry.into());
        let mut task_state = TaskState::get();
        let task = task_state.current_task.as_ref().unwrap().clone();
        let tag = task_state.get_next_tag();

        let timespec = timeout.map(|timeout| {
            Timespec::new()
                .nsec(timeout.subsec_nanos())
                .sec(timeout.as_secs())
        });

        let (handle, previous) = task_state.new_completion(Completion {
            state: MutInPlaceCell::new(CompletionState::Idle {
                entry,
                timespec: timespec.is_some(),
            }),
            scope: MutInPlaceCell::default(),
            owned_resources,
            timespec: timespec.unwrap_or_default(),
            tag,
            task_index: task.task_index,
            iopoll,
        });

        task.register_io(&handle);

        let activity_id = task.activity_id.get();
        task_state.write_event(
            task.task_index,
            Events::IoStart {
                io_type,
                tag,
                fd,
                activity_id,
            },
        );
        drop(task_state);
        drop(previous);

        Self {
            handle: Some(handle),
            _marker: std::marker::PhantomData,
        }
    }

    pub fn cancel(&self) {
        if let Some(handle) = self.handle.as_ref() {
            let mut task_state = TaskState::get();
            handle.cancel(&mut task_state)
        }
    }
}

impl<'a, T: Unpin, C: MakeResult<T>> IsIoPoll for RingFuture<'a, T, C> {
    fn is_io_poll(&self) -> bool {
        if let Some(completion) = self.handle.as_ref() {
            completion.iopoll
        } else {
            false
        }
    }
}

impl<'a, T: Unpin, C: MakeResult<T>> Future for RingFuture<'a, T, C> {
    type Output = Result<T, Errno>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut new_waker = Some(cx.waker().clone());
        let mut task_state = TaskState::get();
        let mut old_waker = None;

        #[cfg(feature = "fault_injection")]
        if let Some((count, fault)) = &mut task_state.fault {
            if *count > 0 {
                *count -= 1;
            } else {
                let fault = *fault;
                task_state.fault = None;
                return Poll::Ready(Err(fault));
            }
        }

        let task_state_ref = &mut *task_state;
        let task = &task_state_ref.current_task;
        let task = task.as_ref().unwrap();
        let stats = &task_state_ref.stats;
        let trace_buffer = &task_state_ref.trace_buffer;
        let ring = &mut task_state_ref.ring;
        let ring_poll = &mut task_state_ref.ring_poll;
        let handle_ref = &mut self.get_mut().handle;
        let completion = handle_ref
            .as_ref()
            .expect("It is illegal to poll a completed future");

        let tag = completion.tag;
        let result = completion.state.use_mut(|state| match state {
            CompletionState::Idle { entry, timespec } => {
                let activity_id = task.activity_id.get();
                let current_task_state = task.get_state();
                if current_task_state == TaskReadyState::Aborted {
                    // If we were aborted while suspended waiting for I/O, then
                    // this is a good time to detect that and panic.
                    panic!("Task aborted");
                }

                let iopoll = completion.iopoll;
                let (flags, entry_count) = if *timespec {
                    (Flags::IO_LINK, 2)
                } else {
                    (Flags::empty(), 1)
                };

                // this clone will be undone when the CQE is processed and Rc::from_raw is called
                let pool_handle_raw_ptr = Rc::into_raw(completion.clone()) as *mut std::ffi::c_void;
                let user_data = io_uring_user_data::from_ptr(pool_handle_raw_ptr);

                let entries = [
                    #[allow(clippy::useless_conversion)]
                    entry
                        .take()
                        .unwrap()
                        .user_data(user_data)
                        .flags(flags)
                        .into(),
                    // We provide a pointer to the timespec in the Completion,
                    // because this always live longer than the pending I/O.
                    #[allow(clippy::useless_conversion)]
                    opcode::LinkTimeout::new(&completion.timespec)
                        .build()
                        // This conversion is for Entry128 when nvme_passthrue is enabled.
                        .into(),
                ];

                if !iopoll {
                    ring.submit(&entries[0..entry_count]);
                    stats.increment_in_flight_io(entry_count as u64)
                } else {
                    ring_poll.submit(&entries[0..entry_count]);
                    stats.increment_in_flight_io_poll(entry_count as u64)
                };

                *state = CompletionState::Submitted {
                    waker: new_waker.take().unwrap(),
                    activity_id,
                    tag,
                    canceled: false,
                };
                Poll::Pending
            }
            CompletionState::Submitted { waker, .. } => {
                let current_task_state = task.get_state();
                if current_task_state == TaskReadyState::Aborted {
                    // If we were aborted while suspended waiting for I/O, then
                    // this is a good time to detect that and panic.
                    panic!("Task aborted");
                }

                // Update the waker in case we were polled from a different task.
                old_waker = Some(std::mem::replace(waker, new_waker.take().unwrap()));

                // still waiting for a completion
                Poll::Pending
            }
            CompletionState::Completed {
                result,
                #[cfg(feature = "io_uring_cmd")]
                big_cqe,
            } => {
                let result = *result;

                #[cfg(not(feature = "io_uring_cmd"))]
                let big_cqe = &[0u64; 2];

                let result = match result {
                    Ok(value) => Ok(C::make_success(value, big_cqe)),
                    Err(code) => {
                        trace_buffer.write_event(
                            task.task_index,
                            Events::IoError {
                                tag,
                                error: code.raw_os_error(),
                                activity_id: task.activity_id.get(),
                            },
                        );
                        Err(code)
                    }
                };
                *state = CompletionState::Terminated;
                Poll::Ready(result)
            }
            CompletionState::Terminated => {
                panic!("RingFuture polled after already returning a result.")
            }
        });

        let unpooled = if matches!(result, Poll::Ready(_)) {
            let completion = handle_ref.take().unwrap();
            task_state.return_completion(completion)
        } else {
            None
        };
        drop(task_state);
        drop(unpooled);
        drop(old_waker);
        drop(new_waker);

        result
    }
}

impl<'a, T: Unpin, C: MakeResult<T>> Drop for RingFuture<'a, T, C> {
    fn drop(&mut self) {
        if let Some(completion) = &self.handle {
            // We got a pending_pool_handle. That means we are being dropped and the I/O
            // has not completed yet. Since no-one will see the result of the I/O, cancel
            // it immediately.
            let mut task_state = TaskState::get();
            completion.cancel(&mut task_state);

            // If we had owned resources registered with the completion, then we can return
            // right away. The resources are guaranteed to live as long as the Rc<Completion>
            // which is always at least as long as until the I/O completes.

            // However, if CompletionResources is None, then this request might have borrowed
            // resources, and we need to block until the I/O is complete.  Otherwise the kernel
            // might try and read and write to the memory for this request and after this drop
            // there is no guarantee it is still valid. We need to wait for the I/O to complete
            // before we return from the drop call.

            // TODO: when AsyncDrop lands in stable, we should see if we can make use of that to
            // improve this code to allow other tasks to continue while waiting for the cancelation.
            fn pending_io_with_borrowed_resources(
                state: &MutInPlaceCell<CompletionState>,
                owned_resources: &CompletionResources,
            ) -> bool {
                state.use_mut(|state| {
                    match state {
                        // The I/O has been submitted to the kernel but is not yet complete, not safe
                        // unless we own the resources and thus control their lifetime
                        CompletionState::Submitted { .. } => {
                            matches!(
                                owned_resources,
                                CompletionResources::None
                            )
                        },
                        // in Idle, we didn't submit the I/O yet so we are safe
                        CompletionState::Idle { .. } |
                        // Completed and Terminated, the I/O is complete so we are safe
                        CompletionState::Completed { .. } |
                        CompletionState::Terminated => false,
                    }
                })
            }

            if pending_io_with_borrowed_resources(&completion.state, &completion.owned_resources) {
                let current_task = task_state.current_task.as_ref().unwrap();
                let task_id = current_task.task_index;
                task_state.write_event(
                    task_id,
                    Events::FutureCanceled {
                        activity_id: current_task.activity_id.get(),
                    },
                );

                while pending_io_with_borrowed_resources(
                    &completion.state,
                    &completion.owned_resources,
                ) {
                    let iopoll = completion.iopoll;
                    task_state = submit_and_complete_io(task_state, false, iopoll);
                }
            }
        }
    }
}

impl<'a, T: Unpin, C: MakeResult<T>> futures::future::FusedFuture for RingFuture<'a, T, C> {
    fn is_terminated(&self) -> bool {
        if let Some(completion) = self.handle.as_ref() {
            completion
                .state
                .use_mut(|state| matches!(state, CompletionState::Terminated))
        } else {
            true
        }
    }
}

pub struct ResultToUnit {}

impl MakeResult<()> for ResultToUnit {
    fn make_success(_value: u32, _cqe: &[u64; 2]) {}
}

pub struct ResultToUsize {}

impl MakeResult<usize> for ResultToUsize {
    fn make_success(value: u32, _cqe: &[u64; 2]) -> usize {
        value as usize
    }
}

pub struct ResultToOwnedFd {}

impl MakeResult<OwnedFd> for ResultToOwnedFd {
    fn make_success(value: u32, _cqe: &[u64; 2]) -> OwnedFd {
        // SAFETY: origination of actual file descriptor. For safe usage
        // it is required that FdFuture only be initialized with an entry
        // top that returns a file descriptor in its result.
        unsafe { OwnedFd::from_raw_fd(value as i32) }
    }
}

#[cfg(feature = "io_uring_cmd")]
pub struct ResultToCqe {}

#[cfg(feature = "io_uring_cmd")]
impl MakeResult<[u64; 2]> for ResultToCqe {
    fn make_success(_value: u32, cqe: &[u64; 2]) -> [u64; 2] {
        *cqe
    }
}

#[cfg(test)]
mod test {
    use crate::{AsyncEvent, Errno, OwnedFd, operations};
    use std::rc::Rc;

    #[crate::test]
    async fn io_scope_cancel_request_keeps_submitted_original_registered() {
        let (fd, peer) = crate::pipe::bipipe();
        let mut bytes = [0; 1];
        operations::io_scope(async || {
            let mut read = Box::pin(operations::read(&fd, &mut bytes));
            assert!(futures::poll!(read.as_mut()).is_pending());
            let completion = read.handle.as_ref().unwrap().clone();
            {
                let mut state = crate::task::TaskState::get();
                completion.cancel(&mut state);
                let owners = Rc::strong_count(&completion);
                completion.cancel(&mut state);
                assert_eq!(
                    Rc::strong_count(&completion),
                    owners,
                    "duplicate cancel ACK owner"
                );
            }
            assert!(completion.scope.use_mut(|scope| scope.is_some()));
            completion.state.use_mut(|state| {
                assert!(matches!(
                    state,
                    crate::CompletionState::Submitted { canceled: true, .. }
                ))
            });
            assert_eq!(read.await, Err(Errno::CANCELED));
            assert!(completion.scope.use_mut(|scope| scope.is_none()));
        })
        .await;
        operations::write(&peer, b"s").await.unwrap();
        assert_eq!(operations::read(&fd, &mut bytes).await.unwrap(), 1);
        assert_eq!(bytes, *b"s");
        operations::close(fd).await.unwrap();
        operations::close(peer).await.unwrap();
    }

    #[crate::test]
    async fn io_scope_acknowledgement_owner_prevents_reuse_in_both_settlement_orders() {
        use crate::{Completion, task::TaskState};
        for acknowledgement_first in [true, false] {
            operations::io_scope(async || {
                for _ in 0..128 {
                    let mut original = Box::pin(operations::nop());
                    let completion = original.handle.as_ref().unwrap();
                    let pointer = Rc::as_ptr(completion);
                    let acknowledgement = completion.acknowledgement_owner();
                    assert_ne!(acknowledgement.addr() & crate::CANCEL_ACK_TAG, 0);
                    // The test controls release of the same ownership token used
                    // by a cancel ACK. The original result comes from a real CQE.
                    let target =
                        acknowledgement.map_addr(|address| address & !crate::CANCEL_ACK_TAG);
                    // SAFETY: this consumes the one owner transferred above.
                    let target = unsafe { Rc::from_raw(target.cast::<Completion>()) };
                    assert!(futures::poll!(original.as_mut()).is_pending());
                    if acknowledgement_first {
                        assert!(target.scope.use_mut(|scope| scope.is_some()));
                        let unpooled = TaskState::get().return_completion(target);
                        drop(unpooled);
                        assert!(
                            original
                                .handle
                                .as_ref()
                                .unwrap()
                                .scope
                                .use_mut(|scope| scope.is_some())
                        );
                        original.await.unwrap();
                    } else {
                        original.await.unwrap();
                        assert!(target.scope.use_mut(|scope| scope.is_none()));
                        assert_eq!(Rc::strong_count(&target), 1);
                        assert!(
                            !TaskState::get()
                                .completion_pool
                                .iter()
                                .any(|entry| Rc::as_ptr(entry) == pointer)
                        );
                        let next = operations::nop();
                        assert_ne!(Rc::as_ptr(next.handle.as_ref().unwrap()), pointer);
                        next.await.unwrap();
                        let unpooled = TaskState::get().return_completion(target);
                        drop(unpooled);
                    }
                }
            })
            .await;
        }
    }

    #[crate::test]
    async fn io_scope_panic_settles_borrowed_storage_before_unwinding() {
        use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
        use std::panic::AssertUnwindSafe;
        let (fd, peer) = crate::pipe::bipipe();
        let mut buffer = [0; 1];
        let mut completion = None;
        let mut pending = FuturesUnordered::new();
        let result = AssertUnwindSafe(operations::io_scope(async || {
            let read = operations::read(&fd, &mut buffer);
            completion = read.handle.clone();
            pending.push(read);
            assert!(futures::poll!(pending.next()).is_pending());
            panic!("deliberate scope panic");
        }))
        .catch_unwind()
        .await;
        assert!(result.is_err());
        completion.unwrap().state.use_mut(|state| {
            assert!(matches!(
                state,
                crate::CompletionState::Completed {
                    result: Err(Errno::CANCELED),
                    ..
                }
            ))
        });
        assert_eq!(pending.next().await, Some(Err(Errno::CANCELED)));
        drop(pending);
        buffer.fill(b'x');
        operations::write(&peer, b"p").await.unwrap();
        assert_eq!(operations::read(&fd, &mut buffer).await.unwrap(), 1);
        assert_eq!(buffer, *b"p");
        operations::close(fd).await.unwrap();
        operations::close(peer).await.unwrap();
    }

    #[crate::test]
    async fn io_scope_owned_completion_resources_drop_outside_task_state_on_reuse() {
        use crate::{CompletionResources, io_type::IOType, task::TaskState};
        use std::cell::Cell;
        struct Reenter(Rc<Cell<usize>>);
        impl Drop for Reenter {
            fn drop(&mut self) {
                let task_state = TaskState::get();
                drop(task_state);
                drop(operations::nop());
                self.0.set(self.0.get() + 1);
            }
        }
        let old_pool = std::mem::take(&mut TaskState::get().completion_pool);
        drop(old_pool);
        let drops = Rc::new(Cell::new(0));
        operations::io_scope(async || {
            super::UnitFuture::with_polled(
                rustix_uring::opcode::Nop::new().build(),
                0,
                None,
                IOType::Nop,
                false,
                CompletionResources::Box(Box::new(Reenter(drops.clone()))),
            )
            .await
            .unwrap();
            assert_eq!(drops.get(), 0, "the unique completion is pooled");
            operations::nop().await.unwrap();
            assert_eq!(drops.get(), 1);
        })
        .await;
    }

    #[crate::test]
    async fn io_scope_drop_settles_borrowed_storage_with_wrapped_waker() {
        use futures::{FutureExt, StreamExt, stream::FuturesUnordered};

        let (fd, peer) = crate::pipe::bipipe();
        let event = AsyncEvent::new();
        let mut buffer = [0; 1];
        let mut completion = None;
        let mut pending = FuturesUnordered::new();
        let mut scope = Box::pin(operations::io_scope(async || {
            let read = operations::read(&fd, &mut buffer);
            completion = read.handle.clone();
            pending.push(read.map(|result| result.map(|_| ())).boxed_local());
            pending.push(
                event
                    .wait()
                    .map(|result| result.map_err(|_| Errno::CANCELED))
                    .boxed_local(),
            );
            assert!(futures::poll!(pending.next()).is_pending());
            futures::future::pending::<()>().await;
        }));
        assert!(futures::poll!(scope.as_mut()).is_pending());
        drop(scope);

        // The read future still owns its borrow. Scope cleanup must already
        // have received its CQE before returning, not just submitted a cancel.
        let completion = completion.unwrap();
        completion.state.use_mut(|state| {
            assert!(matches!(
                state,
                crate::CompletionState::Completed {
                    result: Err(Errno::CANCELED),
                    ..
                }
            ));
        });
        assert_eq!(pending.next().await, Some(Err(Errno::CANCELED)));
        assert_eq!(pending.next().await, Some(Err(Errno::CANCELED)));
        assert_eq!(pending.next().await, None);
        drop(pending);
        buffer.fill(b'x');

        assert_eq!(operations::write(&peer, b"s").await, Ok(1));
        assert_eq!(operations::read(&fd, &mut buffer).await, Ok(1));
        assert_eq!(buffer, [b's']);
    }

    #[crate::test]
    async fn select_test() {
        use futures::select;
        let mut f1 = crate::operations::yield_io();
        let mut f2 = crate::operations::yield_io();
        let mut f3 = crate::operations::yield_io();
        let mut sum = 0;
        loop {
            sum += select! {
                _ = f1 => 1,
                _ = f2 => 2,
                _ = f3 => 4,
                complete => break,
            };
        }
        assert_eq!(7, sum);
    }

    #[crate::test]
    async fn sleep_test() {
        crate::operations::sleep(std::time::Duration::from_secs(0))
            .await
            .unwrap()
    }

    struct TestFuture {
        fd: OwnedFd,
        buf: [u8; 1],
    }

    impl TestFuture {
        async fn read(&mut self) -> Result<usize, Errno> {
            operations::read(&self.fd, &mut self.buf).await
        }
    }

    #[crate::test]
    async fn complete_future_on_different_task_test() {
        use futures::{FutureExt, select};
        let (pipe1, pipe2) = crate::pipe::bipipe();

        let mut fut1 = Box::pin(
            async move {
                let mut test = TestFuture {
                    fd: pipe1,
                    buf: [0; 1],
                };
                test.read().await
            }
            .fuse(),
        );

        let fut2 = Box::pin(crate::operations::nop());

        // this will poll fut1 but complete fut2
        let _ignored = select! {
            _a = fut1 => 1,
            _b = fut2.fuse() => 2,
        };

        // now transfer fut1 into a task to complete it for real.
        let mut task = {
            let ready = Rc::new(AsyncEvent::new());
            let ready_copy = ready.clone();
            let task = operations::spawn_task(async move {
                ready.set();
                let result = fut1.await.unwrap();
                assert_eq!(result, 1, "expected to read 1 byte");
            });
            ready_copy.wait().await.unwrap();
            task
        };

        operations::write(&pipe2, b"1").await.unwrap();

        let joined = select! {
            _ = task => true,
            _ = operations::sleep(std::time::Duration::from_secs(5)).fuse() => false,
        };

        assert!(joined);
    }

    #[crate::test]
    async fn futures_unordered_test() {
        use futures::stream::FuturesUnordered;
        use futures::stream::StreamExt;
        let mut futures = FuturesUnordered::new();
        futures.push(crate::operations::nop());
        futures.push(crate::operations::nop());
        StreamExt::next(&mut futures).await.unwrap().unwrap();
        StreamExt::next(&mut futures).await.unwrap().unwrap();
        assert!(StreamExt::next(&mut futures).await.is_none());
    }

    #[crate::test]
    async fn futures_unordered_event_test() {
        use futures::stream::FuturesUnordered;
        use futures::stream::StreamExt;
        let event = Rc::new(AsyncEvent::new());
        let mut futures = FuturesUnordered::new();
        futures.push(event.wait());
        let task = {
            let event = event.clone();
            operations::spawn_task(async move {
                event.set();
            })
        };
        StreamExt::next(&mut futures).await.unwrap().unwrap();
        assert!(StreamExt::next(&mut futures).await.is_none());
        task.await.unwrap();
    }

    struct WakerFuture;
    impl std::future::Future for WakerFuture {
        type Output = ();

        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            cx.waker().wake_by_ref();
            std::task::Poll::Ready(())
        }
    }

    #[crate::test]
    async fn schedule_completed_test() {
        // This will schedule this task without suspending.  We then
        // immediatley complete this task by returning resulting in this
        // task being schedule but in the Complete state.
        WakerFuture.await;
    }
}
