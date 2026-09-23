use std::{future::Future, io::IoSlice, rc::Rc};

use futures::future::{Either, select};
use kimojio::{AsyncEvent, CancellationToken, Errno, OwnedFd, SplittableStream, operations};
use kimojio_fsm_http1::{IoError, IoErrorKind, IoResult};

use crate::io::{ReadTransport, WriteTransport};

pub(crate) trait Transport {
    type Reader: ReadTransport;
    type Writer: WriteTransport;

    async fn split(self) -> Result<(Self::Reader, Self::Writer), Errno>;
}

pub(crate) struct StreamTransport<S>(pub Box<S>);

impl<S: SplittableStream> Transport for StreamTransport<S> {
    type Reader = S::ReadStream;
    type Writer = S::WriteStream;

    async fn split(self) -> Result<(Self::Reader, Self::Writer), Errno> {
        (*self.0).split().await
    }
}

pub(crate) struct NativeTransport(pub OwnedFd);

pub(crate) struct NativeReader {
    fd: Option<Rc<OwnedFd>>,
    stopped: Rc<AsyncEvent>,
}

pub(crate) struct NativeWriter {
    fd: Option<Rc<OwnedFd>>,
    reader_stopped: Rc<AsyncEvent>,
}

impl Transport for NativeTransport {
    type Reader = NativeReader;
    type Writer = NativeWriter;

    async fn split(self) -> Result<(Self::Reader, Self::Writer), Errno> {
        let fd = Rc::new(self.0);
        let stopped = Rc::new(AsyncEvent::new());
        Ok((
            NativeReader {
                fd: Some(fd.clone()),
                stopped: stopped.clone(),
            },
            NativeWriter {
                fd: Some(fd),
                reader_stopped: stopped,
            },
        ))
    }
}

impl Drop for NativeReader {
    fn drop(&mut self) {
        self.fd.take();
        self.stopped.set();
    }
}

impl ReadTransport for NativeReader {
    async fn receive(&mut self, buffer: &mut [u8], cancel: &CancellationToken) -> IoResult<usize> {
        if cancel.is_cancelled() {
            return Err(native_error(Errno::CANCELED));
        }
        let original = operations::read(self.fd.as_ref().unwrap().as_ref(), buffer);
        settle_one(original, cancel, |original| original.cancel())
            .await
            .map_err(native_error)
    }
}

impl WriteTransport for NativeWriter {
    async fn transmit<'a>(
        &'a mut self,
        slices: &'a mut [IoSlice<'a>],
        cancel: &'a CancellationToken,
    ) -> IoResult<usize> {
        if cancel.is_cancelled() {
            return Err(native_error(Errno::CANCELED));
        }
        let original =
            operations::writev_with_timeout(self.fd.as_ref().unwrap().as_ref(), slices, None, None);
        settle_one(original, cancel, |original| original.cancel())
            .await
            .map_err(native_error)
    }

    async fn close_transport(&mut self) -> IoResult<()> {
        if self.fd.is_none() {
            return Ok(());
        }
        // The driver stops the reader only after its original operation settles.
        // Its destructor releases the other fd owner before signaling this event.
        self.reader_stopped
            .wait()
            .await
            .map_err(|_| native_error(Errno::CANCELED))?;
        let fd = Rc::try_unwrap(self.fd.take().unwrap())
            .expect("native reader retained the descriptor after stopping");
        operations::close(fd).await.map_err(native_error)
    }
}

pub(crate) fn native_error(error: Errno) -> IoError {
    IoError {
        kind: match error {
            Errno::CANCELED => IoErrorKind::Cancelled,
            Errno::INTR => IoErrorKind::Interrupted,
            _ => IoErrorKind::Other,
        },
        code: Some(error.raw_os_error()),
    }
}

async fn settle_one<T, F>(
    mut original: F,
    cancellation: &CancellationToken,
    cancel: impl FnOnce(&mut F),
) -> Result<T, Errno>
where
    F: Future<Output = Result<T, Errno>> + Unpin,
{
    match select(&mut original, cancellation.cancelled()).await {
        Either::Left((result, _)) => result,
        Either::Right((_, original)) => {
            cancel(original);
            // The original CQE, including a late success, owns the result.
            original.await
        }
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
