use std::{
    cell::Cell,
    future::Future,
    io::IoSlice,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use kimojio::{CancellationToken, OwnedFd, Receiver, Sender, operations};
use kimojio_fsm_http1::{CloseOp, ReadCompletion, ReadOp, WriteOp};

use crate::{
    Error,
    body::OutgoingData,
    io::{Pending, WriteAction, WriteResult},
    transport::native_error,
};

pub(crate) trait IoDriver {
    fn read(&mut self, op: ReadOp<Vec<u8>>) -> Result<(), Error>;
    fn write(&mut self, op: WriteOp<OutgoingData>) -> Result<(), Error>;
    fn cancel_read(&self);
    fn cancel_write(&self);
    fn close(&mut self, op: CloseOp) -> Result<(), Error>;
    fn completions(
        &mut self,
        runnable: bool,
    ) -> (
        impl Future<Output = ReadCompletion<Vec<u8>>> + '_,
        impl Future<Output = WriteResult> + '_,
    );
}

pub(crate) fn poll_receive<T>(
    receiver: &Receiver<T>,
    wait: Pin<&mut impl Future<Output = Result<T, kimojio::ChannelError>>>,
    cx: &mut Context<'_>,
    runnable: bool,
) -> Poll<Result<T, kimojio::ChannelError>> {
    if !runnable {
        return wait.poll(cx);
    }
    // A runnable turn cannot suspend, so an empty channel needs no wake registration.
    match receiver.try_recv() {
        Ok(Some(value)) => Poll::Ready(Ok(value)),
        Ok(None) => Poll::Pending,
        Err(error) => Poll::Ready(Err(error)),
    }
}

pub(crate) struct WorkerIo {
    pub read_send: Option<Sender<Pending<ReadOp<Vec<u8>>>>>,
    pub write_send: Sender<WriteAction>,
    pub read_done: Receiver<ReadCompletion<Vec<u8>>>,
    pub write_done: Receiver<WriteResult>,
    pub read_cancel: Option<Rc<CancellationToken>>,
    pub write_cancel: Option<Rc<CancellationToken>>,
}

impl IoDriver for WorkerIo {
    fn read(&mut self, op: ReadOp<Vec<u8>>) -> Result<(), Error> {
        let cancel = Rc::new(CancellationToken::new());
        self.read_cancel = Some(cancel.clone());
        self.read_send
            .as_ref()
            .ok_or(Error::Closed)?
            .try_send(Pending { op, cancel })
            .map_err(|_| Error::Closed)
    }

    fn write(&mut self, op: WriteOp<OutgoingData>) -> Result<(), Error> {
        let cancel = Rc::new(CancellationToken::new());
        self.write_cancel = Some(cancel.clone());
        self.write_send
            .try_send(WriteAction::Write(Pending { op, cancel }))
            .map_err(|_| Error::Closed)
    }

    fn cancel_read(&self) {
        if let Some(cancel) = &self.read_cancel {
            cancel.cancel();
        }
    }

    fn cancel_write(&self) {
        if let Some(cancel) = &self.write_cancel {
            cancel.cancel();
        }
    }

    fn close(&mut self, op: CloseOp) -> Result<(), Error> {
        self.read_send.take();
        self.write_send
            .try_send(WriteAction::Close(op))
            .map_err(|_| Error::Closed)
    }

    fn completions(
        &mut self,
        runnable: bool,
    ) -> (
        impl Future<Output = ReadCompletion<Vec<u8>>> + '_,
        impl Future<Output = WriteResult> + '_,
    ) {
        let Self {
            read_send,
            read_done,
            write_done,
            read_cancel,
            write_cancel,
            ..
        } = self;
        let read = async move {
            if read_send.is_some() {
                let mut wait = std::pin::pin!(read_done.recv());
                let result = futures::future::poll_fn(|cx| {
                    poll_receive(read_done, wait.as_mut(), cx, runnable)
                })
                .await;
                if let Ok(completion) = result {
                    read_cancel.take();
                    return completion;
                }
            }
            std::future::pending().await
        };
        let write = async move {
            let mut wait = std::pin::pin!(write_done.recv());
            let result = futures::future::poll_fn(|cx| {
                poll_receive(write_done, wait.as_mut(), cx, runnable)
            })
            .await;
            if let Ok(completion) = result {
                write_cancel.take();
                return completion;
            }
            std::future::pending().await
        };
        (read, write)
    }
}

/// One pinned allocation per connection slot, reused for every operation.
struct Slot<F, Make> {
    future: Pin<Box<Option<F>>>,
    make: Make,
}

impl<F: Future, Make> Slot<F, Make> {
    fn new(make: Make) -> Self {
        Self {
            future: Box::pin(None),
            make,
        }
    }

    fn start<Input>(&mut self, input: Input)
    where
        Make: Fn(Input) -> F,
    {
        assert!(self.future.is_none(), "operation slot is still occupied");
        self.future.as_mut().set(Some((self.make)(input)));
    }

    #[inline(always)]
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<F::Output> {
        let Some(future) = self.future.as_mut().as_pin_mut() else {
            return Poll::Pending;
        };
        let result = future.poll(cx);
        if result.is_ready() {
            self.future.as_mut().set(None);
        }
        result
    }
}

type ReadInput = (Rc<OwnedFd>, ReadOp<Vec<u8>>, Rc<Cell<bool>>);
type WriteInput = (Rc<OwnedFd>, NativeWrite, Rc<Cell<bool>>);

#[expect(
    clippy::large_enum_variant,
    reason = "The pinned connection slot stores the original operation without allocating per write."
)]
enum NativeWrite {
    Write(WriteOp<OutgoingData>),
    Close(CloseOp),
}

struct NativeIo<R, RM, W, WM> {
    read: Slot<R, RM>,
    write: Slot<W, WM>,
    fd: Option<Rc<OwnedFd>>,
    read_cancel: Rc<Cell<bool>>,
    write_cancel: Rc<Cell<bool>>,
}

pub(crate) fn native_io(fd: OwnedFd) -> impl IoDriver {
    NativeIo {
        read: Slot::new(read_once),
        write: Slot::new(write_once),
        fd: Some(Rc::new(fd)),
        read_cancel: Rc::new(Cell::new(false)),
        write_cancel: Rc::new(Cell::new(false)),
    }
}

impl<R, RM, W, WM> IoDriver for NativeIo<R, RM, W, WM>
where
    R: Future<Output = ReadCompletion<Vec<u8>>>,
    RM: Fn(ReadInput) -> R,
    W: Future<Output = WriteResult>,
    WM: Fn(WriteInput) -> W,
{
    fn read(&mut self, op: ReadOp<Vec<u8>>) -> Result<(), Error> {
        let fd = self.fd.as_ref().ok_or(Error::Closed)?.clone();
        self.read_cancel.set(false);
        self.read.start((fd, op, self.read_cancel.clone()));
        Ok(())
    }

    fn write(&mut self, op: WriteOp<OutgoingData>) -> Result<(), Error> {
        let fd = self.fd.as_ref().ok_or(Error::Closed)?.clone();
        self.write_cancel.set(false);
        self.write
            .start((fd, NativeWrite::Write(op), self.write_cancel.clone()));
        Ok(())
    }

    fn cancel_read(&self) {
        self.read_cancel.set(true);
    }

    fn cancel_write(&self) {
        self.write_cancel.set(true);
    }

    fn close(&mut self, op: CloseOp) -> Result<(), Error> {
        assert!(self.read.future.is_none(), "close preceded read settlement");
        let fd = self.fd.take().ok_or(Error::Closed)?;
        self.write
            .start((fd, NativeWrite::Close(op), self.write_cancel.clone()));
        Ok(())
    }

    fn completions(
        &mut self,
        _runnable: bool,
    ) -> (
        impl Future<Output = ReadCompletion<Vec<u8>>> + '_,
        impl Future<Output = WriteResult> + '_,
    ) {
        (
            futures::future::poll_fn(|cx| self.read.poll(cx)),
            futures::future::poll_fn(|cx| self.write.poll(cx)),
        )
    }
}

async fn read_once((fd, mut op, cancel): ReadInput) -> ReadCompletion<Vec<u8>> {
    let result = if cancel.get() {
        Err(kimojio::Errno::CANCELED)
    } else {
        let original = operations::read(fd.as_ref(), op.bytes_mut());
        settle(original, &cancel, |original| original.cancel()).await
    };
    op.complete(result.map_err(native_error))
}

async fn write_once((fd, action, cancel): WriteInput) -> WriteResult {
    match action {
        NativeWrite::Write(op) => {
            let result = if cancel.get() {
                Err(kimojio::Errno::CANCELED)
            } else {
                let slices = op.slices().map(IoSlice::new);
                let original = operations::writev_with_timeout(fd.as_ref(), &slices, None, None);
                settle(original, &cancel, |original| original.cancel()).await
            };
            WriteResult::Write(op.complete(result.map_err(native_error)))
        }
        NativeWrite::Close(op) => {
            let fd = Rc::try_unwrap(fd).expect("close preceded original-operation settlement");
            WriteResult::Close(op.complete(operations::close(fd).await.map_err(native_error)))
        }
    }
}

async fn settle<T, F>(
    mut original: F,
    requested: &Cell<bool>,
    cancel: impl FnOnce(&mut F),
) -> Result<T, kimojio::Errno>
where
    F: Future<Output = Result<T, kimojio::Errno>> + Unpin,
{
    let mut cancel = Some(cancel);
    futures::future::poll_fn(|cx| {
        if requested.get()
            && let Some(cancel) = cancel.take()
        {
            cancel(&mut original);
        }
        Pin::new(&mut original).poll(cx)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runnable_worker_completion_probes_do_not_register_runtime_waits() {
        let (read_send, _reads) = kimojio::async_channel();
        let (write_send, _writes) = kimojio::async_channel();
        let (_read_complete, read_done) = kimojio::async_channel();
        let (_write_complete, write_done) = kimojio::async_channel();
        let mut io = WorkerIo {
            read_send: Some(read_send),
            write_send,
            read_done,
            write_done,
            read_cancel: None,
            write_cancel: None,
        };
        let (read, write) = io.completions(true);
        let mut read = std::pin::pin!(read);
        let mut write = std::pin::pin!(write);
        let mut cx = Context::from_waker(std::task::Waker::noop());
        // Registering a native channel wait outside a runtime would panic.
        assert!(read.as_mut().poll(&mut cx).is_pending());
        assert!(write.as_mut().poll(&mut cx).is_pending());
    }

    #[test]
    fn reusable_slot_keeps_one_pinned_allocation_for_repeated_operations() {
        let mut slot = Slot::new(std::future::ready);
        let address = slot.future.as_ref().get_ref() as *const _;
        let mut cx = Context::from_waker(std::task::Waker::noop());
        for value in 0..1024 {
            slot.start(value);
            assert_eq!(slot.poll(&mut cx), Poll::Ready(value));
            assert_eq!(slot.poll(&mut cx), Poll::Pending);
            assert_eq!(slot.future.as_ref().get_ref() as *const _, address);
        }
    }

    #[test]
    fn pending_slot_survives_dropped_completion_observers() {
        async fn operation(polls: Rc<Cell<usize>>) -> usize {
            futures::future::poll_fn(|_| {
                polls.set(polls.get() + 1);
                if polls.get() < 3 {
                    Poll::Pending
                } else {
                    Poll::Ready(polls.get())
                }
            })
            .await
        }
        let polls = Rc::new(Cell::new(0));
        let mut slot = Slot::new(operation);
        slot.start(polls.clone());
        let mut cx = Context::from_waker(std::task::Waker::noop());
        for _ in 0..2 {
            let mut observer = std::pin::pin!(futures::future::poll_fn(|cx| slot.poll(cx)));
            assert!(observer.as_mut().poll(&mut cx).is_pending());
        }
        assert_eq!(slot.poll(&mut cx), Poll::Ready(3));
        assert_eq!(polls.get(), 3);
    }

    #[test]
    fn cancellation_is_sent_once_and_keeps_late_success() {
        struct Original(usize);
        impl Future for Original {
            type Output = Result<usize, kimojio::Errno>;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                self.0 += 1;
                if self.0 == 3 {
                    Poll::Ready(Ok(7))
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        }
        let cancel_count = Cell::new(0);
        let requested = Cell::new(true);
        assert_eq!(
            futures::executor::block_on(settle(Original(0), &requested, |_| {
                cancel_count.set(cancel_count.get() + 1);
            })),
            Ok(7)
        );
        assert_eq!(cancel_count.get(), 1);
    }
}
