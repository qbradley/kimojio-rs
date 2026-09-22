use super::tests::full_body_client;
use super::*;

#[test]
fn shared_eager_body_keeps_address_through_partial_writes_and_cancellation() {
    for cancelled in [false, true] {
        let (mut machine, exchange) = full_body_client(false);
        let bytes: Rc<[u8]> = Rc::from(b"abc".as_slice());
        let mut body = OutgoingBody::shared(bytes.clone());
        assert!(admit_eager(&mut machine, exchange, &mut body, 1024, true));
        drop(body);
        let Machine::Client(mut client) = machine else {
            unreachable!()
        };
        let op = loop {
            match client.next(&mut Ports::default()) {
                Some(Event::Write(op)) => break op,
                Some(Event::Deadline(_) | Event::SourceFinished(_)) => {}
                _ => panic!("expected eager write"),
            }
        };
        assert_eq!(op.slices()[1].as_ptr(), bytes.as_ptr());
        assert_eq!(Rc::strong_count(&bytes), 2);
        let head_len = op.slices()[0].len();
        client
            .complete_write(op.complete(Ok(head_len + 1)))
            .unwrap();
        let op = loop {
            match client.next(&mut Ports::default()) {
                Some(Event::Write(op)) => break op,
                Some(Event::Deadline(_)) => {}
                _ => panic!("expected partial continuation"),
            }
        };
        assert_eq!(op.slices()[1], b"bc");
        assert_eq!(op.slices()[1].as_ptr(), bytes.as_ptr().wrapping_add(1));
        if cancelled {
            client.cancel_exchange(exchange).unwrap();
        }
        client.complete_write(op.complete(Ok(2))).unwrap();
        let sent = loop {
            match client.next(&mut Ports::default()) {
                Some(Event::BodySent(sent)) => break sent,
                Some(Event::Deadline(_) | Event::Cancel(_)) => {}
                _ => panic!("expected receipt"),
            }
        };
        assert_eq!(sent.accepted, 3);
        assert_eq!(
            sent.result,
            if cancelled {
                Err(core::Failure::Cancelled)
            } else {
                Ok(())
            }
        );
        assert_eq!(sent.buffer.as_ref().as_ptr(), bytes.as_ptr());
        assert_eq!(Rc::strong_count(&bytes), 2);
        drop(sent);
        assert_eq!(Rc::strong_count(&bytes), 1);
    }
}

#[test]
fn shared_eager_rejection_is_transactional_and_bounds_the_whole_allocation() {
    for (expect, capacity) in [(true, 1024), (false, 2)] {
        let (mut machine, exchange) = full_body_client(expect);
        let bytes: Rc<[u8]> = Rc::from(b"abc".as_slice());
        let mut body = OutgoingBody::shared(bytes.clone());
        assert!(!admit_eager(
            &mut machine,
            exchange,
            &mut body,
            capacity,
            true
        ));
        let Some(Ok(SourceFrame::Data(data))) = futures::executor::block_on(body.source.next())
        else {
            panic!()
        };
        assert_eq!(data.as_ref().as_ptr(), bytes.as_ptr());
        assert_eq!(data.retained_capacity(), bytes.len());
        drop(body);
        assert_eq!(Rc::strong_count(&bytes), 2);
        drop(data);
        assert_eq!(Rc::strong_count(&bytes), 1);
    }
}
