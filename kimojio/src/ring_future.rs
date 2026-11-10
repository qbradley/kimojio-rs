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
use crate::{
    BufferResult, CompletionBuffer, CompletionResources, CompletionState, Errno, MutInPlaceCell,
};
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
    #[cfg(feature = "fault_injection")]
    fault: Option<Errno>,
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
        Self::with_resources(
            |_| entry.into(),
            fd,
            timeout,
            io_type,
            iopoll,
            owned_resources,
        )
    }

    fn with_resources(
        make_entry: impl FnOnce(&CompletionResources) -> SQE,
        fd: i32,
        timeout: Option<Duration>,
        io_type: IOType,
        iopoll: bool,
        owned_resources: CompletionResources,
    ) -> Self {
        let mut task_state = TaskState::get();
        let task = task_state.current_task.as_ref().unwrap().clone();
        let tag = task_state.get_next_tag();

        let timespec = timeout.map(|timeout| {
            Timespec::new()
                .nsec(timeout.subsec_nanos())
                .sec(timeout.as_secs())
        });

        let handle = task_state.new_completion(Completion {
            state: MutInPlaceCell::new(CompletionState::Idle {
                entry: None,
                timespec: timespec.is_some(),
            }),
            owned_resources,
            timespec: timespec.unwrap_or_default(),
            tag,
            task_index: task.task_index,
            iopoll,
        });

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

        let entry = make_entry(&handle.owned_resources);
        handle.state.use_mut(|state| {
            let CompletionState::Idle { entry: slot, .. } = state else {
                unreachable!()
            };
            *slot = Some(entry);
        });
        task.register_io(&handle);

        Self {
            handle: Some(handle),
            #[cfg(feature = "fault_injection")]
            fault: None,
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

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut task_state = TaskState::get();

        #[cfg(feature = "fault_injection")]
        if let Some((count, fault)) = &mut task_state.fault {
            if *count > 0 {
                *count -= 1;
            } else {
                let fault = *fault;
                task_state.fault = None;
                self.fault = Some(fault);
                self.handle.as_ref().unwrap().cancel(&mut task_state);
            }
        }

        let task_state_ref = &mut *task_state;
        let task = &task_state_ref.current_task;
        let task = task.as_ref().unwrap();
        let stats = &task_state_ref.stats;
        let trace_buffer = &task_state_ref.trace_buffer;
        let ring = &mut task_state_ref.ring;
        let ring_poll = &mut task_state_ref.ring_poll;
        let this = self.as_mut().get_mut();
        let handle_ref = &mut this.handle;
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
                    waker: cx.waker().clone(),
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
                waker.clone_from(cx.waker());

                // still waiting for a completion
                Poll::Pending
            }
            CompletionState::Completed {
                result,
                #[cfg(feature = "io_uring_cmd")]
                big_cqe,
            } => {
                let result = *result;
                completion.finish_close(result);

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

        if matches!(result, Poll::Ready(_)) {
            let completion = handle_ref.take().unwrap();
            task_state.return_completion(completion);
        }

        #[cfg(feature = "fault_injection")]
        if result.is_ready()
            && let Some(fault) = this.fault.take()
        {
            return Poll::Ready(Err(fault));
        }

        result
    }
}

/// Parks a buffer at a stable address owned by the completion until the CQE arrives.
pub(crate) struct BufferFuture<B: 'static> {
    future: Option<UsizeFuture<'static>>,
    completion: Option<Rc<Completion>>,
    _marker: std::marker::PhantomData<fn() -> B>,
}

impl<B: 'static> BufferFuture<B> {
    pub(crate) fn new<Entry: Into<SQE>>(
        buffer: B,
        make_entry: impl FnOnce(&mut B) -> Entry,
        fd: i32,
        timeout: Option<Duration>,
        io_type: IOType,
    ) -> Self {
        let future = UsizeFuture::with_resources(
            |resources| {
                let CompletionResources::Buffer(buffer) = resources else {
                    unreachable!()
                };
                // Construct pointers only after the owner reaches its final
                // allocation. Even inline buffers then stay valid across moves.
                let mut buffer = buffer.buffer.borrow_mut();
                let buffer = buffer.as_deref_mut().unwrap().downcast_mut::<B>().unwrap();
                make_entry(buffer).into()
            },
            fd,
            timeout,
            io_type,
            false,
            CompletionResources::Buffer(CompletionBuffer {
                buffer: std::cell::RefCell::new(Some(Box::new(buffer))),
                abandoned: std::cell::Cell::new(false),
            }),
        );
        let completion = future.handle.as_ref().unwrap().clone();
        Self {
            future: Some(future),
            completion: Some(completion),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<B: 'static> Future for BufferFuture<B> {
    type Output = BufferResult<usize, B>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = std::task::ready!(Pin::new(this.future.as_mut().unwrap()).poll(cx));
        let completion = this.completion.take().unwrap();
        let CompletionResources::Buffer(buffer) = &completion.owned_resources else {
            unreachable!()
        };
        let buffer = buffer.buffer.borrow_mut().take().unwrap();
        TaskState::get().return_completion(completion);
        let buffer = *buffer.downcast::<B>().unwrap_or_else(|_| unreachable!());
        Poll::Ready((result, buffer))
    }
}

impl<B: 'static> Drop for BufferFuture<B> {
    fn drop(&mut self) {
        if let Some(completion) = self.completion.take() {
            let CompletionResources::Buffer(buffer) = &completion.owned_resources else {
                unreachable!()
            };
            buffer.abandoned.set(true);
            drop(self.future.take());
            // Buffer destructors can call runtime operations.
            drop(completion.take_abandoned_buffer());
        }
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

            // Owned buffers can outlive the future through Rc<Completion>.
            // Descriptor ownership still requires the actual completion result.

            // However, if CompletionResources is None, then this request might have borrowed
            // resources, and we need to block until the I/O is complete.  Otherwise the kernel
            // might try and read and write to the memory for this request and after this drop
            // there is no guarantee it is still valid. We need to wait for the I/O to complete
            // before we return from the drop call.

            // TODO: when AsyncDrop lands in stable, we should see if we can make use of that to
            // improve this code to allow other tasks to continue while waiting for the cancelation.
            fn pending_io_with_borrowed_resources<T>(
                state: &MutInPlaceCell<CompletionState>,
                owned_resources: &CompletionResources,
            ) -> bool {
                state.use_mut(|state| {
                    match state {
                        // The I/O has been submitted to the kernel but is not yet complete, not safe
                        // unless we own the resources and thus control their lifetime
                        CompletionState::Submitted { .. } => {
                            std::mem::needs_drop::<T>() || matches!(
                                owned_resources,
                                CompletionResources::None | CompletionResources::CloseFd(_)
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

            if pending_io_with_borrowed_resources::<T>(
                &completion.state,
                &completion.owned_resources,
            ) {
                let current_task = task_state.current_task.as_ref().unwrap();
                let task_id = current_task.task_index;
                task_state.write_event(
                    task_id,
                    Events::FutureCanceled {
                        activity_id: current_task.activity_id.get(),
                    },
                );

                while pending_io_with_borrowed_resources::<T>(
                    &completion.state,
                    &completion.owned_resources,
                ) {
                    let iopoll = completion.iopoll;
                    task_state = submit_and_complete_io(task_state, false, iopoll);
                }
            }

            let abandoned = completion.state.use_mut(|state| {
                if let CompletionState::Completed {
                    result,
                    #[cfg(feature = "io_uring_cmd")]
                    big_cqe,
                } = state
                {
                    completion.finish_close(*result);
                    #[cfg(not(feature = "io_uring_cmd"))]
                    let big_cqe = &[0; 2];
                    let result = result.map(|value| C::make_success(value, big_cqe));
                    *state = CompletionState::Terminated;
                    Some(result)
                } else {
                    None
                }
            });
            drop(task_state);
            drop(abandoned);
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
    use std::{cell::Cell, rc::Rc};

    async fn completion_result<T: Unpin, C: super::MakeResult<T>>(
        future: &super::RingFuture<'_, T, C>,
    ) -> Result<u32, crate::Errno> {
        completion_result_for(future.handle.as_ref().unwrap()).await
    }

    async fn completion_result_for(completion: &crate::Completion) -> Result<u32, crate::Errno> {
        for _ in 0..1000 {
            operations::nop().await.unwrap();
            if let Some(result) = completion.state.use_mut(|state| {
                if let crate::CompletionState::Completed { result, .. } = state {
                    Some(*result)
                } else {
                    None
                }
            }) {
                return result;
            }
        }
        panic!("I/O did not complete");
    }

    struct TrackedBuffer {
        bytes: [u8; 16],
        drops: Rc<Cell<usize>>,
    }

    impl AsMut<[u8]> for TrackedBuffer {
        fn as_mut(&mut self) -> &mut [u8] {
            &mut self.bytes
        }
    }

    impl Drop for TrackedBuffer {
        fn drop(&mut self) {
            // Buffer destruction must run outside the runtime's state borrow.
            drop(crate::task::TaskState::get());
            self.drops.set(self.drops.get() + 1);
        }
    }

    fn owned_read(
        fd: OwnedFd,
        drops: Rc<Cell<usize>>,
    ) -> super::BufferFuture<(Rc<OwnedFd>, TrackedBuffer)> {
        use rustix::fd::AsRawFd;
        let raw_fd = fd.as_raw_fd();
        super::BufferFuture::new(
            (
                Rc::new(fd),
                TrackedBuffer {
                    bytes: [0; 16],
                    drops,
                },
            ),
            |(_, buffer)| {
                rustix_uring::opcode::Read::new(
                    rustix_uring::types::Fd(raw_fd),
                    buffer.bytes.as_mut_ptr(),
                    buffer.bytes.len() as u32,
                )
                .build()
            },
            raw_fd,
            None,
            crate::io_type::IOType::Read,
        )
    }

    #[crate::test]
    async fn unpolled_owned_buffer_is_released() {
        let (fd, _peer) = crate::pipe::bipipe();
        let drops = Rc::new(Cell::new(0));
        drop(owned_read(fd, drops.clone()));
        assert_eq!(drops.get(), 1);
    }

    #[crate::test]
    async fn dropped_owned_buffer_waits_for_the_target_cqe() {
        let (fd, _peer) = crate::pipe::bipipe();
        let drops = Rc::new(Cell::new(0));
        let mut read = Box::pin(owned_read(fd, drops.clone()));
        let completion = read.completion.as_ref().unwrap().clone();
        assert!(futures::poll!(read.as_mut()).is_pending());
        drop(read);
        assert_eq!(drops.get(), 0);
        assert!(
            completion
                .state
                .use_mut(|state| matches!(state, crate::CompletionState::Submitted { .. }))
        );
        assert_eq!(
            completion_result_for(&completion).await,
            Err(Errno::CANCELED)
        );
        assert_eq!(drops.get(), 1);
    }

    #[crate::test]
    async fn io_scope_does_not_retain_completed_abandoned_buffers() {
        operations::io_scope(async || {
            let (fd, _peer) = crate::pipe::bipipe();
            let drops = Rc::new(Cell::new(0));
            let mut read = Box::pin(owned_read(fd, drops.clone()));
            let completion = read.completion.as_ref().unwrap().clone();
            assert!(futures::poll!(read.as_mut()).is_pending());
            drop(read);
            assert_eq!(drops.get(), 0);
            assert_eq!(
                completion_result_for(&completion).await,
                Err(Errno::CANCELED)
            );
            assert_eq!(drops.get(), 1);
        })
        .await;
    }

    #[crate::test]
    async fn forgotten_owned_buffer_remains_valid_and_can_complete() {
        let (fd, peer) = crate::pipe::bipipe();
        let drops = Rc::new(Cell::new(0));
        let mut read = Box::pin(owned_read(fd, drops.clone()));
        let completion = read.completion.as_ref().unwrap().clone();
        assert!(futures::poll!(read.as_mut()).is_pending());
        let allocation: *mut super::BufferFuture<(Rc<OwnedFd>, TrackedBuffer)> =
            read.as_mut().get_mut();
        std::mem::forget(read);
        assert_eq!(drops.get(), 0);
        operations::write(&peer, b"safe").await.unwrap();
        assert_eq!(completion_result_for(&completion).await, Ok(4));
        assert_eq!(drops.get(), 0);

        // SAFETY: this is the Box deliberately forgotten above. Its pinned
        // allocation has not moved, and the kernel has finished accessing it.
        let read = unsafe { Box::from_raw(allocation) };
        let (result, (_fd, buffer)) = read.await;
        assert_eq!(result, Ok(4));
        assert_eq!(&buffer.bytes[..4], b"safe");
        drop(buffer);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn shutdown_drains_abandoned_owned_buffers() {
        let drops = Rc::new(Cell::new(0));
        let task_drops = drops.clone();
        crate::run_test("shutdown_drains_abandoned_owned_buffers", async move {
            let (fd, _peer) = crate::pipe::bipipe();
            let mut read = Box::pin(owned_read(fd, task_drops.clone()));
            assert!(futures::poll!(read.as_mut()).is_pending());
            drop(read);
            assert_eq!(task_drops.get(), 0);
        });
        assert_eq!(drops.get(), 1);
    }

    #[cfg(feature = "fault_injection")]
    #[crate::test]
    async fn injected_fault_does_not_return_a_submitted_owned_buffer() {
        let (fd, _peer) = crate::pipe::bipipe();
        let drops = Rc::new(Cell::new(0));
        let mut read = Box::pin(owned_read(fd, drops.clone()));
        let completion = read.completion.as_ref().unwrap().clone();
        assert!(futures::poll!(read.as_mut()).is_pending());
        operations::inject_fault(0, Errno::FAULT);
        assert!(futures::poll!(read.as_mut()).is_pending());
        assert_eq!(drops.get(), 0);
        let (result, buffer) = read.await;
        assert_eq!(result, Err(Errno::FAULT));
        assert!(
            completion
                .state
                .use_mut(|state| matches!(state, crate::CompletionState::Terminated))
        );
        drop(buffer);
        assert_eq!(drops.get(), 1);
    }

    #[crate::test]
    async fn unpolled_close_releases_descriptor() {
        let (fd, peer) = crate::pipe::bipipe();
        drop(operations::close(fd));
        assert_eq!(
            rustix::net::recv(&peer, &mut [0; 1], rustix::net::RecvFlags::DONTWAIT),
            Ok((0, 0))
        );
    }

    #[crate::test]
    async fn canceled_close_releases_descriptor() {
        let (fd, peer) = crate::pipe::bipipe();
        let close = operations::close(fd);
        close.cancel();
        assert_eq!(close.await, Err(Errno::CANCELED));
        assert_eq!(
            rustix::net::recv(&peer, &mut [0; 1], rustix::net::RecvFlags::DONTWAIT),
            Ok((0, 0))
        );

        let (fd, peer) = crate::pipe::bipipe();
        {
            let mut close = std::pin::pin!(operations::close(fd));
            assert!(futures::poll!(close.as_mut()).is_pending());
        }
        assert_eq!(
            rustix::net::recv(&peer, &mut [0; 1], rustix::net::RecvFlags::DONTWAIT),
            Ok((0, 0))
        );
    }

    #[crate::test]
    async fn completed_close_does_not_close_reused_number() {
        use std::os::fd::AsRawFd;
        let (fd, peer) = crate::pipe::bipipe();
        // Avoid ordinary descriptor allocations without exceeding the soft limit.
        let min_fd = rustix::process::getrlimit(rustix::process::Resource::Nofile)
            .current
            .map_or(10000, |limit| {
                i32::try_from((limit / 2).min(10000)).unwrap()
            });
        let fd = rustix::io::fcntl_dupfd_cloexec(&fd, min_fd).unwrap();
        let raw = fd.as_raw_fd();
        let mut close = Box::pin(operations::close(fd));
        assert!(futures::poll!(close.as_mut()).is_pending());
        assert_eq!(completion_result(&close).await, Ok(0));
        let replacement = rustix::io::fcntl_dupfd_cloexec(&peer, raw).unwrap();
        assert_eq!(replacement.as_raw_fd(), raw);
        drop(close);
        assert!(rustix::io::fcntl_getfd(&replacement).is_ok());
    }

    #[crate::test]
    async fn dropped_open_reclaims_unconsumed_result() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = std::ffi::CString::new(file.path().as_os_str().as_encoded_bytes()).unwrap();
        operations::io_scope(async || {
            let mut open = Box::pin(operations::open(
                &path,
                rustix::fs::OFlags::RDONLY,
                rustix::fs::Mode::empty(),
            ));
            assert!(futures::poll!(open.as_mut()).is_pending());
            let raw = completion_result(&open).await.unwrap();
            let link = format!("/proc/self/fd/{raw}");
            assert_eq!(std::fs::read_link(&link).unwrap(), file.path());
            drop(open);
            assert_ne!(std::fs::read_link(&link).ok().as_deref(), Some(file.path()));
        })
        .await;
    }

    #[crate::test]
    async fn dropped_accept_reclaims_unconsumed_result() {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut accept = Box::pin(operations::accept(&listener));
        assert!(futures::poll!(accept.as_mut()).is_pending());
        completion_result(&accept).await.unwrap();
        drop(accept);
        assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    }

    #[crate::test]
    async fn dropped_descriptor_future_drains_owned_resources() {
        let path = Box::new(std::ffi::CString::new("/dev/null").unwrap());
        let entry = rustix_uring::opcode::OpenAt::new(
            rustix_uring::types::Fd(libc::AT_FDCWD),
            path.as_ptr(),
        )
        .flags(rustix::fs::OFlags::RDONLY)
        .build();
        let mut open = Box::pin(super::OwnedFdFuture::with_polled(
            entry,
            -1,
            None,
            crate::io_type::IOType::Open,
            false,
            crate::CompletionResources::Box(path),
        ));
        let completion = open.handle.as_ref().unwrap().clone();
        assert!(futures::poll!(open.as_mut()).is_pending());
        drop(open);
        assert!(
            completion
                .state
                .use_mut(|state| matches!(state, crate::CompletionState::Terminated))
        );
    }

    #[crate::test]
    async fn consumed_descriptor_result_remains_owned_by_caller() {
        let fd = operations::open(
            c"/dev/null",
            rustix::fs::OFlags::RDONLY,
            rustix::fs::Mode::empty(),
        )
        .await
        .unwrap();
        assert!(rustix::io::fcntl_getfd(&fd).is_ok());
        operations::close(fd).await.unwrap();
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
