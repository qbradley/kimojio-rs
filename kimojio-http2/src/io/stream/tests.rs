use std::{io::IoSlice, time::Duration};

use kimojio::{CancellationToken, OwnedFd};

use super::*;

struct DelayedWriter {
    fd: Option<OwnedFd>,
    first: Rc<CancellationToken>,
    release: Rc<CancellationToken>,
    writes: usize,
    first_amount: usize,
    pending: bool,
}

impl AsyncStreamWrite for DelayedWriter {
    async fn write(&mut self, mut bytes: &[u8], deadline: Option<Instant>) -> Result<(), Errno> {
        while !bytes.is_empty() {
            self.writes += 1;
            self.pending = true;
            let result =
                operations::write_with_deadline(self.fd.as_ref().unwrap(), bytes, deadline).await;
            self.pending = false;
            let n = result?;
            assert_ne!(n, 0);
            if self.writes == 1 {
                self.first_amount = n;
                self.first.cancel();
                // Withhold a real positive completion until cancellation is
                // requested. The original can then start another native write.
                let _ = self.release.cancelled().await;
                operations::yield_cpu().await;
            }
            bytes = &bytes[n..];
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), Errno> {
        rustix::net::shutdown(self.fd.as_ref().unwrap(), rustix::net::Shutdown::Write)
    }

    async fn close(&mut self) -> Result<(), Errno> {
        assert!(!self.pending);
        operations::close(self.fd.take().unwrap()).await
    }
}

async fn late_write(case: usize) {
    let (fd, peer) = kimojio::pipe::bipipe();
    rustix::net::sockopt::set_socket_send_buffer_size(&fd, 4096).unwrap();
    let first = Rc::new(CancellationToken::new());
    let release = Rc::new(CancellationToken::new());
    let requested = Cell::new(false);
    let mut writer = DelayedWriter {
        fd: Some(fd),
        first: first.clone(),
        release: release.clone(),
        writes: 0,
        first_amount: 0,
        pending: false,
    };
    let data = vec![1; 65536];
    let mut slices = match case {
        0 => vec![IoSlice::new(b"prefix"), IoSlice::new(&data)],
        1 => vec![IoSlice::new(&data)],
        _ => vec![IoSlice::new(b"prefix")],
    };
    let (other, other_peer) = kimojio::pipe::bipipe();
    let started = CancellationToken::new();
    let result = operations::timeout_at(kimojio::clock_now() + Duration::from_secs(5), async {
        let original = operations::io_scope(async || {
            settle(writer.writev(&mut slices, None), &requested).await
        });
        let cancel = async {
            first.cancelled().await.unwrap();
            started.cancelled().await.unwrap();
            requested.set(true);
            release.cancel();
        };
        let unrelated = operations::io_scope(async || {
            let read = async {
                let mut bytes = [0; 1];
                started.cancel();
                assert_eq!(operations::read(&other, &mut bytes).await.unwrap(), 1);
                assert_eq!(bytes, [42]);
            };
            let write = async {
                release.cancelled().await.unwrap();
                operations::yield_cpu().await;
                assert_eq!(operations::write(&other_peer, &[42]).await.unwrap(), 1);
            };
            futures::join!(read, write);
        });
        let (result, (), ()) = futures::join!(original, cancel, unrelated);
        result
    })
    .await
    .unwrap();
    assert!(requested.get());
    assert!(writer.first_amount > 0);
    if case == 2 {
        assert_eq!(writer.writes, 1);
        assert_eq!(result, Ok(()), "late original success was replaced");
    } else {
        assert!(writer.writes >= 2);
        assert_eq!(result, Err(Errno::CANCELED));
    }
    writer.close().await.unwrap();
    operations::close(peer).await.unwrap();
    operations::close(other).await.unwrap();
    operations::close(other_peer).await.unwrap();
}

#[kimojio::test]
async fn cancellation_settles_write_all_continuations_without_cancelling_siblings() {
    for case in 0..3 {
        late_write(case).await;
    }
}

#[kimojio::test]
async fn cancellation_preserves_a_late_original_read_success() {
    let (fd, peer) = kimojio::pipe::bipipe();
    operations::write(&peer, b"original").await.unwrap();
    let first = CancellationToken::new();
    let release = CancellationToken::new();
    let requested = Cell::new(false);
    let mut bytes = [0; 8];
    let original = operations::io_scope(async || {
        settle(
            async {
                let result = operations::read(&fd, &mut bytes).await;
                first.cancel();
                let _ = release.cancelled().await;
                result
            },
            &requested,
        )
        .await
    });
    let cancel = async {
        first.cancelled().await.unwrap();
        requested.set(true);
        release.cancel();
    };
    let (result, ()) = futures::join!(original, cancel);
    assert_eq!(result, Ok(8));
    assert_eq!(bytes, *b"original");
    operations::close(fd).await.unwrap();
    operations::close(peer).await.unwrap();
}
