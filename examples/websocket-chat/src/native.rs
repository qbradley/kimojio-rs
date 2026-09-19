//! Single-operation native primitives. The protocol, never this executor,
//! decides whether a successful partial completion requires another operation.
use std::future::Future;
use std::io::IoSlice;
use std::os::fd::AsFd;

use futures::future::{Either, select};
use kimojio::{CancellationToken, Errno, operations};

/// A revoked worker cannot create another native read.
pub async fn read_once(
    fd: &impl AsFd,
    bytes: &mut [u8],
    cancellation: &CancellationToken,
) -> Result<usize, Errno> {
    if cancellation.is_cancelled() {
        return Err(Errno::CANCELED);
    }
    let original = operations::read(fd, bytes);
    settle_one(original, cancellation, |original| original.cancel()).await
}

/// Submit one writev, return its exact original CQE, and never retry a suffix.
/// The slices and their payloads remain borrowed until the original settles.
pub async fn write_once(
    fd: &impl AsFd,
    slices: &[IoSlice<'_>],
    cancellation: &CancellationToken,
) -> Result<usize, Errno> {
    if cancellation.is_cancelled() {
        return Err(Errno::CANCELED);
    }
    let original = operations::writev_with_timeout(fd, slices, None, None);
    settle_one(original, cancellation, |original| original.cancel()).await
}

pub(crate) async fn settle_one<T, F>(
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
            // Cancellation is a request, not an ownership transfer. A success
            // that wins this race still reports its exact accepted byte count.
            original.await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::{Context, Poll};
    use std::time::Duration;

    #[test]
    fn native_vectored_write_read_and_eof_return_exact_counts() {
        kimojio::run_test("chat_exact_raw_io", async {
            let (sender, receiver) = kimojio::pipe::bipipe();
            let cancellation = CancellationToken::new();
            let slices = [IoSlice::new(b"ab"), IoSlice::new(b""), IoSlice::new(b"cde")];
            assert_eq!(write_once(&sender, &slices, &cancellation).await, Ok(5));
            let mut bytes = [0; 64];
            assert_eq!(read_once(&receiver, &mut bytes, &cancellation).await, Ok(5));
            assert_eq!(&bytes[..5], b"abcde");
            drop(sender);
            assert_eq!(read_once(&receiver, &mut bytes, &cancellation).await, Ok(0));
        });
    }

    #[test]
    fn revocation_prevents_new_native_operations() {
        kimojio::run_test("chat_revoked_raw_io", async {
            let (sender, receiver) = kimojio::pipe::bipipe();
            let cancellation = CancellationToken::new();
            cancellation.cancel();
            assert_eq!(
                write_once(&sender, &[IoSlice::new(b"must not escape")], &cancellation).await,
                Err(Errno::CANCELED)
            );
            let mut bytes = [0xa5; 64];
            assert_eq!(
                read_once(&receiver, &mut bytes, &cancellation).await,
                Err(Errno::CANCELED)
            );
            assert_eq!(bytes, [0xa5; 64]);
            assert_eq!(
                rustix::net::recv(&receiver, &mut bytes, rustix::net::RecvFlags::DONTWAIT),
                Err(Errno::AGAIN)
            );
        });
    }

    #[test]
    fn pending_native_read_keeps_buffer_until_original_cancellation_completion() {
        kimojio::run_test("chat_cancel_pending_read", async {
            let (sender, receiver) = kimojio::pipe::bipipe();
            let cancellation = Rc::new(CancellationToken::new());
            let reader_cancel = cancellation.clone();
            let reader = operations::spawn_task(async move {
                let mut bytes = [0xa5; 64];
                let result = read_once(&receiver, &mut bytes, &reader_cancel).await;
                (result, bytes)
            });
            operations::yield_io().await;
            cancellation.cancel();
            let (result, bytes) = reader.await.unwrap();
            assert_eq!(result, Err(Errno::CANCELED));
            assert_eq!(bytes, [0xa5; 64]);
            let mut eof = [0; 1];
            assert_eq!(
                read_once(&sender, &mut eof, &CancellationToken::new()).await,
                Ok(0)
            );
        });
    }

    #[test]
    fn native_partial_write_never_reissues_after_revocation() {
        kimojio::run_test("chat_cancel_partial_write", async {
            let (sender, receiver) = kimojio::pipe::bipipe();
            rustix::net::sockopt::set_socket_send_buffer_size(&sender, 4096).unwrap();
            let cancellation = Rc::new(CancellationToken::new());
            let writer_cancel = cancellation.clone();
            let writer = operations::spawn_task(async move {
                let bytes = vec![0x5a; 1024 * 1024];
                let slices = [IoSlice::new(&bytes[..7]), IoSlice::new(&bytes[7..])];
                let result = write_once(&sender, &slices, &writer_cancel).await;
                (sender, result)
            });
            operations::sleep(Duration::from_millis(20)).await.unwrap();
            cancellation.cancel();
            let (sender, result) = writer.await.unwrap();
            let count = result.unwrap();
            assert!(count > 0 && count < 1024 * 1024, "{count}");
            assert_eq!(
                write_once(&sender, &[IoSlice::new(b"forbidden retry")], &cancellation).await,
                Err(Errno::CANCELED)
            );
            let mut received = vec![0; count];
            let read = read_once(&receiver, &mut received, &CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(read, count);
            assert!(received.iter().all(|byte| *byte == 0x5a));
            let mut extra = [0; 32];
            assert_eq!(
                rustix::net::recv(&receiver, &mut extra, rustix::net::RecvFlags::DONTWAIT),
                Err(Errno::AGAIN)
            );
        });
    }

    #[test]
    fn blocked_native_write_settles_original_before_payload_return() {
        kimojio::run_test("chat_cancel_blocked_write", async {
            let (sender, receiver) = kimojio::pipe::bipipe();
            rustix::net::sockopt::set_socket_send_buffer_size(&sender, 4096).unwrap();
            let filler = [0x33; 4096];
            loop {
                match rustix::net::send(&sender, &filler, rustix::net::SendFlags::DONTWAIT) {
                    Ok(count) => assert!(count > 0),
                    Err(Errno::AGAIN) => break,
                    other => panic!("cannot fill native socket: {other:?}"),
                }
            }
            let cancellation = Rc::new(CancellationToken::new());
            let writer_cancel = cancellation.clone();
            let entered = Rc::new(Cell::new(false));
            let writer_entered = entered.clone();
            let writer = operations::spawn_task(async move {
                let payload = vec![0xa5; 16384];
                let slices = [IoSlice::new(&payload)];
                writer_entered.set(true);
                let result = write_once(&sender, &slices, &writer_cancel).await;
                (result, payload)
            });
            operations::sleep(Duration::from_millis(20)).await.unwrap();
            assert!(entered.get());
            cancellation.cancel();
            let (result, payload) = writer.await.unwrap();
            assert_eq!(result, Err(Errno::CANCELED));
            assert_eq!(payload, [0xa5; 16384]);
            let mut received = [0; 16384];
            let count = read_once(&receiver, &mut received, &CancellationToken::new())
                .await
                .unwrap();
            assert!(count > 0);
            assert!(received[..count].iter().all(|byte| *byte == 0x33));
        });
    }

    #[test]
    fn successful_original_completion_after_cancel_is_not_rewritten() {
        struct Original {
            canceled: Rc<Cell<bool>>,
            polls: Rc<Cell<usize>>,
        }
        impl Future for Original {
            type Output = Result<usize, Errno>;
            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
                self.polls.set(self.polls.get() + 1);
                if self.canceled.get() {
                    Poll::Ready(Ok(3))
                } else {
                    Poll::Pending
                }
            }
        }
        kimojio::run_test("chat_cancel_success_race", async {
            let cancellation = CancellationToken::new();
            cancellation.cancel();
            let canceled = Rc::new(Cell::new(false));
            let polls = Rc::new(Cell::new(0));
            let original = Original {
                canceled: canceled.clone(),
                polls: polls.clone(),
            };
            assert_eq!(
                settle_one(original, &cancellation, |original| original
                    .canceled
                    .set(true))
                .await,
                Ok(3)
            );
            assert!(canceled.get());
            assert_eq!(polls.get(), 2);
        });
    }
}
