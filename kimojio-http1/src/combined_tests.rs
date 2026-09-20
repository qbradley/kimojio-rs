use super::*;

#[kimojio::test]
async fn both_roles_preserve_default_boundaries_with_unboxed_ready_bodies() {
    for server in [false, true] {
        for coalesce in [false, true] {
            let mut config = Config::new(core::ConnectionId {
                slot: 300,
                generation: 1,
            });
            assert!(!config.coalesce_full_bodies);
            config.coalesce_full_bodies = coalesce;
            let state = State::new(config, server, Shutdown::default()).unwrap();
            assert_eq!(state.coalesce_full_bodies, coalesce);
            let full = OutgoingBody::full(b"ready");
            assert!(matches!(full.source, OutgoingSource::Ready(Some(_))));
            assert!(matches!(
                OutgoingBody::empty().source,
                OutgoingSource::Ready(None)
            ));
        }
    }
}

#[derive(Clone, Copy)]
enum CancelPhase {
    BeforePoll,
    Pending,
    Completed,
}

#[kimojio::test]
async fn eager_native_slots_keep_exact_original_results_across_scheduler_modes() {
    for phase in [
        CancelPhase::BeforePoll,
        CancelPhase::Pending,
        CancelPhase::Completed,
    ] {
        let mut client = core::Client::with_output_type(
            core::ConnectionId {
                slot: 301,
                generation: 1,
            },
            core::Config::default(),
            vec![0; 1024],
            core::Tick(0),
        )
        .unwrap();
        let exchange = client
            .request(core::Request {
                head: core::RequestHead {
                    method: "POST",
                    target: "/",
                    version: core::Version::Http11,
                    headers: &[core::Header {
                        name: "host",
                        value: b"test",
                    }],
                },
                body: core::BodyLength::Known(7),
                expect_continue: false,
            })
            .unwrap();
        let payload = b"payload".to_vec();
        let pointer = payload.as_ptr();
        client
            .send_body_eager(core::SendBody {
                exchange,
                buffer: OutgoingData::Owned(payload),
                range: 0..7,
                end: true,
            })
            .unwrap();
        let op = (0..32)
            .find_map(|_| match client.next(&mut Ports) {
                Some(Event::Write(op)) => Some(op),
                Some(Event::Deadline(_) | Event::SourceFinished(_)) | None => None,
                _ => panic!("unexpected eager admission event"),
            })
            .unwrap();
        assert_eq!(op.slices()[1].as_ptr(), pointer);
        let wire_bytes: usize = op.slices().iter().map(|slice| slice.len()).sum();
        let (fd, peer) = kimojio::pipe::bipipe();
        let mut filled = 0;
        if matches!(phase, CancelPhase::Pending) {
            rustix::net::sockopt::set_socket_send_buffer_size(&fd, 4096).unwrap();
            loop {
                match rustix::net::send(&fd, &[0x33; 4096], rustix::net::SendFlags::DONTWAIT) {
                    Ok(count) => {
                        assert!(count > 0);
                        filled += count;
                    }
                    Err(kimojio::Errno::AGAIN) => break,
                    result => panic!("socket fill failed: {result:?}"),
                }
            }
        }
        let mut io = native_io(fd);
        io.write(op).unwrap();
        if !matches!(phase, CancelPhase::BeforePoll) {
            let mut completion = std::pin::pin!(io.completions(true).1);
            assert!(futures::poll!(completion.as_mut()).is_pending());
        }
        if matches!(phase, CancelPhase::Pending) {
            operations::yield_io().await;
            for runnable in [false, true, false] {
                let mut completion = std::pin::pin!(io.completions(runnable).1);
                assert!(futures::poll!(completion.as_mut()).is_pending());
            }
        }
        if matches!(phase, CancelPhase::Completed) {
            let mut bytes = vec![0; wire_bytes];
            assert_eq!(operations::read(&peer, &mut bytes).await, Ok(wire_bytes));
            assert!(bytes.ends_with(b"payload"));
        }
        client.cancel_exchange(exchange).unwrap();
        io.cancel_write();
        let WriteResult::Write(completion) = io.completions(false).1.await else {
            panic!("missing original write completion");
        };
        client.complete_write(completion).unwrap();
        let receipt = (0..32)
            .find_map(|_| match client.next(&mut Ports) {
                Some(Event::BodySent(receipt)) => Some(receipt),
                Some(Event::Cancel(_) | Event::Deadline(_) | Event::SourceFinished(_)) | None => {
                    None
                }
                _ => panic!("unexpected canceled eager event"),
            })
            .unwrap();
        assert_eq!(receipt.buffer.as_ref().as_ptr(), pointer);
        assert_eq!(receipt.buffer.as_ref(), b"payload");
        assert_eq!(receipt.acceptance, core::Acceptance::Exact);
        assert_eq!(
            receipt.accepted,
            if matches!(phase, CancelPhase::Completed) {
                7
            } else {
                0
            }
        );
        assert_eq!(receipt.result, Err(core::Failure::Cancelled));
        drop(receipt);
        let close = (0..32)
            .find_map(|_| match client.next(&mut Ports) {
                Some(Event::Close(op)) => Some(op),
                Some(Event::Deadline(_) | Event::ExchangeFinished(_)) | None => None,
                _ => panic!("unexpected close event"),
            })
            .unwrap();
        io.close(close).unwrap();
        let WriteResult::Close(completion) = io.completions(false).1.await else {
            panic!("missing actual close");
        };
        client.complete_close(completion).unwrap();
        if filled > 0 {
            let mut bytes = vec![0; filled];
            assert_eq!(operations::read(&peer, &mut bytes).await, Ok(filled));
            assert!(bytes.iter().all(|byte| *byte == 0x33));
        }
        assert_eq!(operations::read(&peer, &mut [0; 1]).await, Ok(0));
    }
}
