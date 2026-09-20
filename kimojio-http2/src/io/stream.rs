use std::{
    cell::Cell,
    future::Future,
    rc::Rc,
    task::{Context, Poll},
    time::Instant,
};

use kimojio::{AsyncStreamRead, AsyncStreamWrite, Errno, operations};
use kimojio_fsm_http2 as core;

use super::{Io, Slot, Timers, Write, WriteDone, failure};
use crate::body::Data;

type ReadInput<R> = ((Box<R>, core::ReadOp), Rc<Cell<bool>>);
type WriteInput<W> = ((Box<W>, Write), Rc<Cell<bool>>);
type ReadDone<R> = (Box<R>, core::ReadCompletion, Option<Errno>);
type StreamWriteDone<W> = (Option<Box<W>>, WriteDone, Option<Errno>);

struct StreamIo<R, W, RF, RM, WF, WM> {
    reader: Option<Box<R>>,
    writer: Option<Box<W>>,
    read: Slot<RF, RM>,
    write: Slot<WF, WM>,
    timers: Timers,
    error: Option<Errno>,
}

pub(crate) fn stream<R: AsyncStreamRead, W: AsyncStreamWrite>(
    reader: R,
    writer: W,
    epoch: Instant,
) -> impl Io {
    StreamIo {
        reader: Some(Box::new(reader)),
        writer: Some(Box::new(writer)),
        read: Slot::new(read_once::<R>),
        write: Slot::new(write_once::<W>),
        timers: Timers::new(epoch),
        error: None,
    }
}

impl<R, W, RF, RM, WF, WM> Io for StreamIo<R, W, RF, RM, WF, WM>
where
    RF: Future<Output = ReadDone<R>>,
    RM: Fn(ReadInput<R>) -> RF,
    WF: Future<Output = StreamWriteDone<W>>,
    WM: Fn(WriteInput<W>) -> WF,
{
    fn read(&mut self, op: core::ReadOp) {
        self.read.start(
            op.token().clone(),
            (self.reader.take().expect("read half available"), op),
        );
    }

    fn write(&mut self, op: core::WriteOp<Data>) {
        self.write.start(
            op.token().clone(),
            (
                self.writer.take().expect("write half available"),
                Write::Data(op),
            ),
        );
    }

    fn close(&mut self, op: core::CloseOp) {
        assert!(self.read.future.is_none(), "close preceded read settlement");
        drop(self.reader.take());
        self.write.start(
            op.token().clone(),
            (self.writer.take().expect("close once"), Write::Close(op)),
        );
    }

    fn wake(&mut self, op: core::WakeOp) {
        self.timers.start(op);
    }

    fn cancel(&mut self, token: &core::Token) {
        if self.read.token.as_ref() == Some(token) {
            self.read.cancel.set(true);
        } else if self.write.token.as_ref() == Some(token) {
            self.write.cancel.set(true);
        } else {
            self.timers.cancel(token);
        }
    }

    fn poll_read(&mut self, cx: &mut Context<'_>) -> Poll<core::ReadCompletion> {
        self.read.poll(cx).map(|(reader, done, error)| {
            self.reader = Some(reader);
            self.error = self.error.or(error);
            done
        })
    }

    fn poll_write(&mut self, cx: &mut Context<'_>) -> Poll<WriteDone> {
        self.write.poll(cx).map(|(writer, done, error)| {
            self.writer = writer;
            self.error = self.error.or(error);
            done
        })
    }

    fn poll_wake(&mut self, cx: &mut Context<'_>) -> Poll<core::WakeCompletion> {
        self.timers.poll(cx)
    }

    fn take_error(&mut self) -> Option<Errno> {
        self.error.take()
    }
}

async fn read_once<R: AsyncStreamRead>(
    ((mut reader, mut op), cancel): ReadInput<R>,
) -> ReadDone<R> {
    let result = if cancel.get() {
        Err(Errno::CANCELED)
    } else {
        operations::io_scope(async || settle(reader.try_read(op.buffer_mut(), None), &cancel).await)
            .await
    };
    let error = source_error(result, &cancel);
    let done = op.complete(match result {
        Ok(0) => core::ReadOutcome::Eof,
        Ok(n) => core::ReadOutcome::Read(n),
        Err(error) => core::ReadOutcome::Failed(failure(error)),
    });
    (reader, done, error)
}

async fn write_once<W: AsyncStreamWrite>(
    ((mut writer, write), cancel): WriteInput<W>,
) -> StreamWriteDone<W> {
    match write {
        Write::Data(op) => {
            // The trait promises all bytes on success, but no count on failure.
            let mut started = false;
            let result = if cancel.get() {
                Err(Errno::CANCELED)
            } else {
                operations::io_scope(async || {
                    let mut slices = op.slices();
                    let offered = slices.iter().map(|slice| slice.len()).sum();
                    started = true;
                    settle(writer.writev(&mut slices, None), &cancel)
                        .await
                        .map(|()| offered)
                })
                .await
            };
            let error = source_error(result, &cancel);
            let done = op.complete(match result {
                Ok(n) => core::WriteOutcome::Written(n),
                Err(error) => core::WriteOutcome::Failed {
                    progress: if started {
                        core::Progress::AtLeast(0)
                    } else {
                        core::Progress::Exact(0)
                    },
                    error: failure(error),
                },
            });
            (Some(writer), WriteDone::Data(done), error)
        }
        Write::Close(op) => {
            let result = writer.close().await;
            (
                None,
                WriteDone::Close(op.complete(result.map_err(failure)), result),
                None,
            )
        }
    }
}

fn source_error<T: Copy>(result: Result<T, Errno>, cancel: &Cell<bool>) -> Option<Errno> {
    result
        .err()
        .filter(|error| *error != Errno::CANCELED || !cancel.get())
}

async fn settle<T>(
    original: impl Future<Output = Result<T, Errno>>,
    requested: &Cell<bool>,
) -> Result<T, Errno> {
    futures::pin_mut!(original);
    futures::future::poll_fn(|cx| {
        let result = original.as_mut().poll(cx);
        if result.is_pending() && requested.get() {
            // Cancel continuations too: write-all can issue more I/O after a
            // positive late completion. Keep the original until it settles.
            operations::io_scope_cancel();
        }
        result
    })
    .await
}

#[cfg(test)]
mod tests;
