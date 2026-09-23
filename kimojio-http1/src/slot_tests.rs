use super::*;
use crate::io_driver::{IoDriver, native_io};

#[kimojio::test]
async fn direct_slots_keep_forwarded_storage_through_cancel_and_late_success() {
    for success in [false, true] {
        let (server, chunk, returned) = lease();
        let pointer = chunk.as_ptr();
        let (mut client, id) = destination();
        client
            .send_body(core::SendBody {
                exchange: id,
                buffer: OutgoingData::Forward(chunk),
                range: 0..7,
                end: false,
            })
            .unwrap();
        let write = (0..32)
            .find_map(|_| match client.next(&mut Ports::default()) {
                Some(Event::Write(op)) => Some(op),
                Some(Event::SourceFinished(_) | Event::Deadline(_)) | None => None,
                _ => panic!("unexpected write event"),
            })
            .unwrap();
        assert_eq!(write.slices()[1].as_ptr(), pointer);
        let (fd, peer) = kimojio::pipe::bipipe();
        if !success {
            rustix::net::sockopt::set_socket_send_buffer_size(&fd, 4096).unwrap();
            loop {
                match rustix::net::send(&fd, &[0x33; 4096], rustix::net::SendFlags::DONTWAIT) {
                    Ok(count) => assert!(count > 0),
                    Err(kimojio::Errno::AGAIN) => break,
                    result => panic!("socket fill failed: {result:?}"),
                }
            }
        }
        let mut io = native_io(fd);
        io.write(write).unwrap();
        {
            let mut completion = std::pin::pin!(io.completions(true).1);
            assert!(futures::poll!(completion.as_mut()).is_pending());
        }
        operations::yield_io().await;
        if success {
            let mut bytes = [0; 7];
            assert_eq!(operations::read(&peer, &mut bytes).await, Ok(7));
            assert_eq!(&bytes, b"payload");
        }
        assert_not_returned(&returned);
        io.cancel_write();
        let WriteResult::Write(completion) = io.completions(false).1.await else {
            panic!("missing original completion");
        };
        assert_not_returned(&returned);
        client.complete_write(completion).unwrap();
        let receipt = (0..32)
            .find_map(|_| match client.next(&mut Ports::default()) {
                Some(Event::BodySent(receipt)) => Some(receipt),
                Some(Event::SourceFinished(_) | Event::Deadline(_) | Event::Cancel(_)) | None => {
                    None
                }
                _ => panic!("unexpected receipt event"),
            })
            .unwrap();
        assert_eq!(receipt.acceptance, core::Acceptance::Exact);
        assert_eq!(receipt.accepted, if success { 7 } else { 0 });
        assert_eq!(receipt.result.is_ok(), success);
        assert_not_returned(&returned);
        drop(receipt);
        assert_returned(server, returned, pointer);
    }
}

#[kimojio::test]
async fn dropping_a_native_read_slot_settles_before_descriptor_release() {
    let config = core::Config::default();
    let buffer = vec![0; config.max_buffer_bytes];
    let mut server = core::Server::with_output_type(
        core::ConnectionId {
            slot: 180,
            generation: 1,
        },
        config,
        buffer,
        core::Tick(0),
    )
    .unwrap();
    let read = (0..32)
        .find_map(|_| match server.next(&mut Ports::default()) {
            Some(Event::Read(op)) => Some(op),
            Some(Event::Deadline(_)) | None => None,
            _ => panic!("unexpected server event"),
        })
        .unwrap();
    let (fd, peer) = kimojio::pipe::bipipe();
    let mut io = native_io(fd);
    io.read(read).unwrap();
    for _ in 0..3 {
        let mut completion = std::pin::pin!(io.completions(true).0);
        assert!(futures::poll!(completion.as_mut()).is_pending());
    }
    drop(io);
    assert_eq!(operations::read(&peer, &mut [0; 1]).await, Ok(0));
}
