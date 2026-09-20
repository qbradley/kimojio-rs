use std::{future::Future, io::IoSlice, rc::Rc};

use kimojio::{AsyncStreamRead, AsyncStreamWrite, CancellationToken, Receiver, Sender, operations};
use kimojio_fsm_http1::{
    CloseCompletion, CloseOp, IoError, IoErrorKind, IoResult, ReadCompletion, ReadOp,
    WriteCompletion, WriteOp,
};

use crate::body::OutgoingData;

pub(crate) struct Pending<T> {
    pub op: T,
    pub cancel: Rc<CancellationToken>,
}

#[expect(
    clippy::large_enum_variant,
    reason = "The single-slot channel keeps operations inline instead of allocating per write."
)]
pub(crate) enum WriteAction {
    Write(Pending<WriteOp<OutgoingData>>),
    Close(CloseOp),
}

#[expect(
    clippy::large_enum_variant,
    reason = "The single-slot channel returns the inline operation without a per-receipt allocation."
)]
pub(crate) enum WriteResult {
    Write(WriteCompletion<OutgoingData>),
    Close(CloseCompletion),
}

pub(crate) fn transport_error(error: kimojio::Errno) -> IoError {
    // Stream adapters perform their own retry/readiness handling.
    IoError {
        kind: if error == kimojio::Errno::CANCELED {
            IoErrorKind::Cancelled
        } else {
            IoErrorKind::Other
        },
        code: Some(error.raw_os_error()),
    }
}

fn write_error(error: kimojio::Errno) -> IoError {
    IoError {
        kind: if error == kimojio::Errno::CANCELED {
            IoErrorKind::CancelledUnknownProgress
        } else {
            IoErrorKind::UnknownProgress
        },
        code: Some(error.raw_os_error()),
    }
}

fn cancelled<T>() -> IoResult<T> {
    Err(IoError {
        kind: IoErrorKind::Cancelled,
        code: None,
    })
}

async fn await_io<T>(
    io: impl Future<Output = Result<T, kimojio::Errno>>,
    cancel: &CancellationToken,
) -> Result<T, kimojio::Errno> {
    let cancellation = cancel.cancelled();
    futures::pin_mut!(io, cancellation);
    let mut cancelling = false;
    futures::future::poll_fn(|cx| {
        let result = io.as_mut().poll(cx);
        if result.is_pending() {
            if !cancelling {
                cancelling = cancellation.as_mut().poll(cx).is_ready();
            }
            if cancelling {
                // A positive partial completion can submit new write-all operations.
                operations::io_scope_cancel();
            }
        }
        result
    })
    .await
}

pub(crate) trait ReadTransport {
    async fn receive(&mut self, buffer: &mut [u8], cancel: &CancellationToken) -> IoResult<usize>;
}

pub(crate) trait WriteTransport {
    async fn transmit<'a>(
        &'a mut self,
        slices: &'a mut [IoSlice<'a>],
        cancel: &'a CancellationToken,
    ) -> IoResult<usize>;

    async fn close_transport(&mut self) -> IoResult<()>;
}

impl<R: AsyncStreamRead> ReadTransport for R {
    async fn receive(&mut self, buffer: &mut [u8], cancel: &CancellationToken) -> IoResult<usize> {
        if cancel.is_cancelled() {
            cancelled()
        } else {
            operations::io_scope(async || {
                await_io(self.try_read(buffer, None), cancel)
                    .await
                    .map_err(transport_error)
            })
            .await
        }
    }
}

impl<W: AsyncStreamWrite> WriteTransport for W {
    async fn transmit<'a>(
        &'a mut self,
        slices: &'a mut [IoSlice<'a>],
        cancel: &'a CancellationToken,
    ) -> IoResult<usize> {
        if cancel.is_cancelled() {
            cancelled()
        } else {
            operations::io_scope(async || {
                let offered = slices.iter().map(|bytes| bytes.len()).sum();
                await_io(self.writev(slices, None), cancel)
                    .await
                    .map(|()| offered)
                    .map_err(write_error)
            })
            .await
        }
    }

    async fn close_transport(&mut self) -> IoResult<()> {
        AsyncStreamWrite::close(self).await.map_err(transport_error)
    }
}

pub(crate) async fn read_worker<R: ReadTransport>(
    mut stream: Box<R>,
    requests: Receiver<Pending<ReadOp<Vec<u8>>>>,
    completions: Sender<ReadCompletion<Vec<u8>>>,
) {
    while let Ok(Pending { mut op, cancel }) = requests.recv().await {
        let result = stream.receive(op.bytes_mut(), &cancel).await;
        if completions.send(op.complete(result)).await.is_err() {
            break;
        }
    }
}

pub(crate) async fn write_worker<W: WriteTransport>(
    mut stream: Box<W>,
    requests: Receiver<WriteAction>,
    completions: Sender<WriteResult>,
) {
    let mut closed = false;
    while let Ok(action) = requests.recv().await {
        let result = match action {
            WriteAction::Write(Pending { op, cancel }) => {
                let result = {
                    let mut slices = op.slices().map(IoSlice::new);
                    stream.transmit(&mut slices, &cancel).await
                };
                WriteResult::Write(op.complete(result))
            }
            WriteAction::Close(op) => {
                closed = true;
                WriteResult::Close(op.complete(stream.close_transport().await))
            }
        };
        if completions.send(result).await.is_err() || closed {
            break;
        }
    }
    if !closed {
        let _ = stream.close_transport().await;
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        time::{Duration, Instant},
    };

    use super::*;

    struct DelayedCompletionWriter {
        fd: Option<kimojio::OwnedFd>,
        first_completed: Rc<CancellationToken>,
        cancel: Rc<CancellationToken>,
        writes: usize,
        native_writes: usize,
        first_amount: usize,
        pending: Rc<Cell<bool>>,
    }

    impl AsyncStreamWrite for DelayedCompletionWriter {
        async fn write(
            &mut self,
            mut buffer: &[u8],
            deadline: Option<Instant>,
        ) -> Result<(), kimojio::Errno> {
            self.writes += 1;
            while !buffer.is_empty() {
                self.native_writes += 1;
                self.pending.set(true);
                let result =
                    operations::write_with_deadline(self.fd.as_ref().unwrap(), buffer, deadline)
                        .await;
                self.pending.set(false);
                let amount = result?;
                assert_ne!(amount, 0);
                if self.native_writes == 1 {
                    self.first_amount = amount;
                    self.first_completed.cancel();
                    let _ = self.cancel.cancelled().await;
                    // Deliver the positive completion only after the worker requests cancellation.
                    operations::yield_cpu().await;
                }
                buffer = &buffer[amount..];
            }
            Ok(())
        }

        async fn shutdown(&mut self) -> Result<(), kimojio::Errno> {
            rustix::net::shutdown(self.fd.as_ref().unwrap(), rustix::net::Shutdown::Write)
        }

        async fn close(&mut self) -> Result<(), kimojio::Errno> {
            assert!(
                !self.pending.get(),
                "close preceded original I/O settlement"
            );
            operations::close(self.fd.take().unwrap()).await
        }
    }

    #[derive(Clone, Copy)]
    enum CompletionCase {
        NextSlice,
        PartialWrite,
        Complete,
    }

    async fn unrelated_connection(
        started: Rc<CancellationToken>,
        cancelled: Rc<CancellationToken>,
    ) {
        let (client_fd, server_fd) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let config = |slot| {
            crate::Config::new(kimojio_fsm_http1::ConnectionId {
                slot,
                generation: 1,
            })
        };
        let (mut client, driver) =
            crate::connect(kimojio::OwnedFdStream::new(client_fd), config(96));
        let server = crate::serve_connection(
            kimojio::OwnedFdStream::new(server_fd),
            config(97),
            move |_| {
                let started = started.clone();
                let cancelled = cancelled.clone();
                async move {
                    assert!(!cancelled.is_cancelled());
                    started.cancel();
                    let _ = cancelled.cancelled().await;
                    Ok(http::Response::new(crate::OutgoingBody::full(
                        b"unrelated connection".to_vec(),
                    )))
                }
            },
        );
        let app = async {
            let request = http::Request::builder()
                .uri("/")
                .header("host", "test")
                .body(crate::OutgoingBody::empty())
                .unwrap();
            let mut response = client.send(request).await.unwrap();
            assert_eq!(
                response.body_mut().collect(64).await.unwrap(),
                b"unrelated connection"
            );
            client.shutdown().await.unwrap();
        };
        let ((), driver, server) = futures::join!(app, driver.run(), server);
        driver.unwrap();
        server.unwrap();
    }

    async fn late_write_completion(case: CompletionCase) {
        let (fd, peer) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        rustix::net::sockopt::set_socket_send_buffer_size(&fd, 4096).unwrap();
        let first_completed = Rc::new(CancellationToken::new());
        let cancel = Rc::new(CancellationToken::new());
        let other_started = Rc::new(CancellationToken::new());
        let mut writer = DelayedCompletionWriter {
            fd: Some(fd),
            first_completed: first_completed.clone(),
            cancel: cancel.clone(),
            writes: 0,
            native_writes: 0,
            first_amount: 0,
            pending: Rc::new(Cell::new(false)),
        };
        let remaining = vec![b'x'; 64 * 1024];
        let mut slices = match case {
            CompletionCase::NextSlice => {
                vec![IoSlice::new(b"prefix"), IoSlice::new(&remaining)]
            }
            CompletionCase::PartialWrite => vec![IoSlice::new(&remaining)],
            CompletionCase::Complete => vec![IoSlice::new(b"prefix")],
        };
        let count = slices.len();
        let result = operations::timeout_at(kimojio::clock_now() + Duration::from_secs(1), async {
            let write = operations::io_scope(async || {
                await_io(writer.writev(&mut slices, None), &cancel).await
            });
            let cancellation = async {
                let _ = first_completed.cancelled().await;
                let _ = other_started.cancelled().await;
                cancel.cancel();
            };
            let (result, (), ()) = futures::join!(
                write,
                cancellation,
                unrelated_connection(other_started.clone(), cancel.clone())
            );
            result
        })
        .await
        .expect("write-all continuation escaped cancellation");
        assert_eq!(writer.writes, count);
        match case {
            CompletionCase::Complete => {
                assert_eq!(writer.native_writes, 1);
                assert_eq!(writer.first_amount, b"prefix".len());
                assert_eq!(result, Ok(()));
            }
            CompletionCase::NextSlice | CompletionCase::PartialWrite => {
                assert!(writer.native_writes >= 2);
                assert!(writer.first_amount > 0 && writer.first_amount < remaining.len());
                assert_eq!(result, Err(kimojio::Errno::CANCELED));
            }
        }
        writer.close().await.unwrap();
        operations::close(peer).await.unwrap();
    }

    #[kimojio::test]
    async fn wrapped_wakers_settle_positive_write_completions() {
        use futures::{StreamExt, stream::FuturesUnordered};

        let mut connections = FuturesUnordered::new();
        for case in [
            CompletionCase::NextSlice,
            CompletionCase::PartialWrite,
            CompletionCase::Complete,
        ] {
            connections.push(late_write_completion(case));
        }
        let mut completed = 0;
        while connections.next().await.is_some() {
            completed += 1;
        }
        assert_eq!(completed, 3);
    }

    #[kimojio::test]
    async fn cancellation_covers_write_all_continuations_after_positive_progress() {
        late_write_completion(CompletionCase::NextSlice).await;
    }

    #[kimojio::test]
    async fn cancellation_covers_replacement_write_after_native_partial_success() {
        late_write_completion(CompletionCase::PartialWrite).await;
    }

    #[kimojio::test]
    async fn cancellation_preserves_a_late_complete_write_success() {
        late_write_completion(CompletionCase::Complete).await;
    }

    #[test]
    fn every_native_write_error_preserves_unknown_progress() {
        for error in [
            kimojio::Errno::AGAIN,
            kimojio::Errno::INTR,
            kimojio::Errno::CONNRESET,
        ] {
            let mapped = write_error(error);
            assert_eq!(mapped.kind, IoErrorKind::UnknownProgress);
            assert_eq!(mapped.code, Some(error.raw_os_error()));
        }
        let mapped = write_error(kimojio::Errno::CANCELED);
        assert_eq!(mapped.kind, IoErrorKind::CancelledUnknownProgress);
        assert_eq!(mapped.code, Some(kimojio::Errno::CANCELED.raw_os_error()));
    }
}
