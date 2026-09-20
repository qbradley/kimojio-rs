use std::{
    cell::Cell,
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
    time::Instant,
};

use futures::{FutureExt, future::LocalBoxFuture};
use kimojio::{OwnedFd, operations};
use kimojio_fsm_http2 as core;

use crate::body::Data;

pub(crate) enum WriteDone {
    Data(core::WriteCompletion<Data>),
    Close(core::CloseCompletion, Result<(), kimojio::Errno>),
}

pub(crate) trait Io {
    fn read(&mut self, op: core::ReadOp);
    fn write(&mut self, op: core::WriteOp<Data>);
    fn close(&mut self, op: core::CloseOp);
    fn wake(&mut self, op: core::WakeOp);
    fn cancel(&mut self, token: &core::Token);
    fn poll_read(&mut self, cx: &mut Context<'_>) -> Poll<core::ReadCompletion>;
    fn poll_write(&mut self, cx: &mut Context<'_>) -> Poll<WriteDone>;
    fn poll_wake(&mut self, cx: &mut Context<'_>) -> Poll<core::WakeCompletion>;
}

struct Slot<F, Make> {
    future: Pin<Box<Option<F>>>,
    make: Make,
    token: Option<core::Token>,
    cancel: Rc<Cell<bool>>,
}

impl<F: Future, Make> Slot<F, Make> {
    fn new(make: Make) -> Self {
        Self {
            future: Box::pin(None),
            make,
            token: None,
            cancel: Rc::new(Cell::new(false)),
        }
    }

    fn start<T>(&mut self, token: core::Token, input: T)
    where
        Make: Fn((T, Rc<Cell<bool>>)) -> F,
    {
        assert!(self.future.is_none(), "original operation is unsettled");
        self.cancel.set(false);
        self.token = Some(token);
        self.future
            .as_mut()
            .set(Some((self.make)((input, self.cancel.clone()))));
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<F::Output> {
        let Some(future) = self.future.as_mut().as_pin_mut() else {
            return Poll::Pending;
        };
        let result = future.poll(cx);
        if result.is_ready() {
            self.future.as_mut().set(None);
            self.token = None;
        }
        result
    }
}

enum Write {
    Data(core::WriteOp<Data>),
    Close(core::CloseOp),
}

type ReadInput = ((Rc<OwnedFd>, core::ReadOp), Rc<Cell<bool>>);
type WriteInput = ((Rc<OwnedFd>, Write), Rc<Cell<bool>>);

struct Timer {
    future: LocalBoxFuture<'static, core::WakeCompletion>,
    cancel: Rc<Cell<bool>>,
}

struct NativeIo<R, RM, W, WM> {
    fd: Option<Rc<OwnedFd>>,
    read: Slot<R, RM>,
    write: Slot<W, WM>,
    timers: BTreeMap<u64, Timer>,
    epoch: Instant,
}

pub(crate) fn native(fd: OwnedFd, epoch: Instant) -> impl Io {
    NativeIo {
        fd: Some(Rc::new(fd)),
        read: Slot::new(read_once),
        write: Slot::new(write_once),
        timers: BTreeMap::new(),
        epoch,
    }
}

impl<R, RM, W, WM> Io for NativeIo<R, RM, W, WM>
where
    R: Future<Output = core::ReadCompletion>,
    RM: Fn(ReadInput) -> R,
    W: Future<Output = WriteDone>,
    WM: Fn(WriteInput) -> W,
{
    fn read(&mut self, op: core::ReadOp) {
        self.read.start(
            op.token().clone(),
            (self.fd.as_ref().expect("open descriptor").clone(), op),
        );
    }
    fn write(&mut self, op: core::WriteOp<Data>) {
        self.write.start(
            op.token().clone(),
            (
                self.fd.as_ref().expect("open descriptor").clone(),
                Write::Data(op),
            ),
        );
    }
    fn close(&mut self, op: core::CloseOp) {
        assert!(self.read.future.is_none(), "close preceded read settlement");
        self.write.start(
            op.token().clone(),
            (self.fd.take().expect("close once"), Write::Close(op)),
        );
    }
    fn wake(&mut self, op: core::WakeOp) {
        let token = op.token().sequence();
        let cancel = Rc::new(Cell::new(false));
        self.timers.insert(
            token,
            Timer {
                future: alarm(op, self.epoch, cancel.clone()).boxed_local(),
                cancel,
            },
        );
    }
    fn cancel(&mut self, token: &core::Token) {
        if self.read.token.as_ref() == Some(token) {
            self.read.cancel.set(true);
        } else if self.write.token.as_ref() == Some(token) {
            self.write.cancel.set(true);
        } else if let Some(timer) = self.timers.get(&token.sequence()) {
            timer.cancel.set(true);
        }
    }
    fn poll_read(&mut self, cx: &mut Context<'_>) -> Poll<core::ReadCompletion> {
        self.read.poll(cx)
    }
    fn poll_write(&mut self, cx: &mut Context<'_>) -> Poll<WriteDone> {
        self.write.poll(cx)
    }
    fn poll_wake(&mut self, cx: &mut Context<'_>) -> Poll<core::WakeCompletion> {
        let ready =
            self.timers
                .iter_mut()
                .find_map(|(id, timer)| match timer.future.as_mut().poll(cx) {
                    Poll::Ready(done) => Some((*id, done)),
                    Poll::Pending => None,
                });
        match ready {
            Some((id, done)) => {
                self.timers.remove(&id);
                Poll::Ready(done)
            }
            None => Poll::Pending,
        }
    }
}

fn failure(error: kimojio::Errno) -> core::IoFailure {
    if error == kimojio::Errno::CANCELED {
        core::IoFailure::Cancelled
    } else {
        core::IoFailure::Failed
    }
}

async fn read_once(((fd, mut op), cancel): ReadInput) -> core::ReadCompletion {
    let result = if cancel.get() {
        Err(kimojio::Errno::CANCELED)
    } else {
        settle(
            operations::read(fd.as_ref(), op.buffer_mut()),
            &cancel,
            |io| io.cancel(),
        )
        .await
    };
    op.complete(match result {
        Ok(0) => core::ReadOutcome::Eof,
        Ok(n) => core::ReadOutcome::Read(n),
        Err(error) => core::ReadOutcome::Failed(failure(error)),
    })
}

async fn write_once(((fd, write), cancel): WriteInput) -> WriteDone {
    match write {
        Write::Data(op) => {
            let result = if cancel.get() {
                Err(kimojio::Errno::CANCELED)
            } else {
                // Borrow only after the original operation occupies its pinned slot.
                let slices = op.slices();
                settle(
                    operations::writev_with_timeout(fd.as_ref(), &slices, None, None),
                    &cancel,
                    |io| io.cancel(),
                )
                .await
            };
            WriteDone::Data(op.complete(match result {
                Ok(n) => core::WriteOutcome::Written(n),
                Err(error) => core::WriteOutcome::Failed {
                    progress: core::Progress::Exact(0),
                    error: failure(error),
                },
            }))
        }
        Write::Close(op) => {
            let fd = Rc::try_unwrap(fd).expect("close preceded original-operation settlement");
            let result = operations::close(fd).await;
            WriteDone::Close(op.complete(result.map_err(failure)), result)
        }
    }
}

async fn alarm(op: core::WakeOp, epoch: Instant, cancel: Rc<Cell<bool>>) -> core::WakeCompletion {
    let Some(deadline) = epoch.checked_add(op.deadline()) else {
        return op.failed(core::IoFailure::Failed);
    };
    let result = if cancel.get() {
        Err(kimojio::Errno::CANCELED)
    } else {
        operations::io_scope(async || {
            let mut sleep = operations::sleep_until(deadline);
            futures::future::poll_fn(|cx| {
                if cancel.get() {
                    Poll::Ready(Err(kimojio::Errno::CANCELED))
                } else {
                    Pin::new(&mut sleep).poll(cx)
                }
            })
            .await
        })
        .await
    };
    match result {
        Ok(()) => op.complete(kimojio::clock_now().saturating_duration_since(epoch)),
        Err(error) => op.failed(failure(error)),
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
    fn cancellation_keeps_original_future_and_late_exact_success() {
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
        let requested = Cell::new(true);
        let cancellations = Cell::new(0);
        let result = futures::executor::block_on(settle(Original(0), &requested, |_| {
            cancellations.set(cancellations.get() + 1);
        }));
        assert_eq!(result, Ok(7));
        assert_eq!(cancellations.get(), 1);
    }
}
