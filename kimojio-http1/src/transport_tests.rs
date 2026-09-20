use std::{
    cell::Cell,
    pin::Pin,
    task::{Context, Poll},
};

use super::*;

#[kimojio::test]
async fn native_reads_only_into_supplied_storage_and_counts_vectored_writes() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (mut reader, mut writer) = NativeTransport(fd).split().await.unwrap();
    let cancel = CancellationToken::new();
    let mut slices = [IoSlice::new(b"ab"), IoSlice::new(b""), IoSlice::new(b"cde")];
    assert_eq!(writer.transmit(&mut slices, &cancel).await, Ok(5));
    let mut received = [0; 16];
    assert_eq!(operations::read(&peer, &mut received).await, Ok(5));
    assert_eq!(&received[..5], b"abcde");

    assert_eq!(operations::write(&peer, b"abcdef").await, Ok(6));
    let mut buffer = [0xa5; 8];
    assert_eq!(reader.receive(&mut buffer[..3], &cancel).await, Ok(3));
    assert_eq!(&buffer[..3], b"abc");
    assert_eq!(&buffer[3..], &[0xa5; 5]);
    assert_eq!(
        rustix::net::recv(&peer, &mut received, rustix::net::RecvFlags::DONTWAIT),
        Err(Errno::AGAIN)
    );
    assert_eq!(
        rustix::net::recv(
            reader.fd.as_ref().unwrap().as_ref(),
            &mut received,
            rustix::net::RecvFlags::DONTWAIT,
        )
        .map(|(count, _)| count),
        Ok(3),
        "the reader must not prefetch a payload into hidden storage"
    );
    assert_eq!(&received[..3], b"def");
    drop(reader);
    writer.close_transport().await.unwrap();
    assert_eq!(operations::read(&peer, &mut received).await, Ok(0));
}

#[kimojio::test]
async fn native_short_write_reports_exact_bytes_without_a_hidden_suffix_retry() {
    let (fd, peer) = kimojio::pipe::bipipe();
    rustix::net::sockopt::set_socket_send_buffer_size(&fd, 4096).unwrap();
    let (reader, mut writer) = NativeTransport(fd).split().await.unwrap();
    let cancel = CancellationToken::new();
    let payload = vec![0x5a; 1024 * 1024];
    let mut slices = [IoSlice::new(&payload[..7]), IoSlice::new(&payload[7..])];
    let count = writer.transmit(&mut slices, &cancel).await.unwrap();
    assert!(count > 0 && count < payload.len(), "{count}");
    let mut received = vec![0; count];
    assert_eq!(operations::read(&peer, &mut received).await, Ok(count));
    assert_eq!(received, payload[..count]);
    assert_eq!(
        rustix::net::recv(&peer, &mut [0; 1], rustix::net::RecvFlags::DONTWAIT),
        Err(Errno::AGAIN)
    );
    drop(reader);
    writer.close_transport().await.unwrap();
}

#[kimojio::test]
async fn native_cancel_settles_original_read_and_prevents_new_operations() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (mut reader, mut writer) = NativeTransport(fd).split().await.unwrap();
    let cancel = CancellationToken::new();
    let mut bytes = [0xa5; 16];
    let mut read = Box::pin(reader.receive(&mut bytes, &cancel));
    assert!(futures::poll!(read.as_mut()).is_pending());
    cancel.cancel();
    assert_eq!(read.await, Err(native_error(Errno::CANCELED)));
    assert_eq!(bytes, [0xa5; 16]);
    assert_eq!(
        reader.receive(&mut bytes, &cancel).await,
        Err(native_error(Errno::CANCELED))
    );
    assert_eq!(
        writer
            .transmit(&mut [IoSlice::new(b"forbidden")], &cancel)
            .await,
        Err(native_error(Errno::CANCELED))
    );
    assert_eq!(
        rustix::net::recv(&peer, &mut bytes, rustix::net::RecvFlags::DONTWAIT),
        Err(Errno::AGAIN)
    );
    drop(reader);
    writer.close_transport().await.unwrap();
    assert_eq!(operations::read(&peer, &mut bytes).await, Ok(0));
}

#[kimojio::test]
async fn native_blocked_write_returns_payload_only_after_original_settlement() {
    let (fd, peer) = kimojio::pipe::bipipe();
    rustix::net::sockopt::set_socket_send_buffer_size(&fd, 4096).unwrap();
    let filler = [0x33; 4096];
    let mut filled = 0;
    loop {
        match rustix::net::send(&fd, &filler, rustix::net::SendFlags::DONTWAIT) {
            Ok(count) => {
                assert!(count > 0);
                filled += count;
            }
            Err(Errno::AGAIN) => break,
            result => panic!("socket fill failed: {result:?}"),
        }
    }
    let (reader, mut writer) = NativeTransport(fd).split().await.unwrap();
    let cancel = CancellationToken::new();
    let payload = vec![0xa5; 16384];
    let mut slices = [IoSlice::new(&payload)];
    let mut write = Box::pin(writer.transmit(&mut slices, &cancel));
    assert!(futures::poll!(write.as_mut()).is_pending());
    cancel.cancel();
    assert_eq!(write.await, Err(native_error(Errno::CANCELED)));
    assert_eq!(payload, [0xa5; 16384]);
    let mut received = vec![0; filled];
    assert_eq!(operations::read(&peer, &mut received).await, Ok(filled));
    assert!(received.iter().all(|byte| *byte == 0x33));
    assert_eq!(
        rustix::net::recv(&peer, &mut [0; 1], rustix::net::RecvFlags::DONTWAIT),
        Err(Errno::AGAIN)
    );
    drop(reader);
    writer.close_transport().await.unwrap();
}

#[kimojio::test]
async fn native_close_waits_for_reader_release_and_really_closes_the_socket() {
    let (fd, peer) = kimojio::pipe::bipipe();
    let (mut reader, mut writer) = NativeTransport(fd).split().await.unwrap();
    let cancel = CancellationToken::new();
    let mut buffer = [0xa5; 16];
    let mut read = Box::pin(reader.receive(&mut buffer, &cancel));
    assert!(futures::poll!(read.as_mut()).is_pending());
    let mut close = Box::pin(writer.close_transport());
    assert!(futures::poll!(close.as_mut()).is_pending());
    assert_eq!(
        rustix::net::recv(&peer, &mut [0; 1], rustix::net::RecvFlags::DONTWAIT),
        Err(Errno::AGAIN)
    );
    drop(read);
    assert_eq!(buffer, [0xa5; 16]);
    drop(reader);
    close.await.unwrap();
    assert_eq!(operations::read(&peer, &mut [0; 1]).await, Ok(0));
    writer.close_transport().await.unwrap();
}

#[kimojio::test]
async fn native_late_success_keeps_the_original_exact_result() {
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
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let canceled = Rc::new(Cell::new(false));
    let polls = Rc::new(Cell::new(0));
    let original = Original {
        canceled: canceled.clone(),
        polls: polls.clone(),
    };
    assert_eq!(
        settle_one(original, &cancellation, |original| {
            original.canceled.set(true);
        })
        .await,
        Ok(3)
    );
    assert!(canceled.get());
    assert_eq!(polls.get(), 2);
}
