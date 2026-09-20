use super::*;
use std::pin::pin;

#[kimojio::test]
async fn native_mailbox_preserves_fifo_wakes_and_worker_bound() {
    for clients in 1..=3 {
        let capacity = 2 * clients + 2;
        let (sender, receiver) = kimojio::async_channel_unbounded_with_capacity(capacity);
        let storage = sender.storage_bytes();
        let mailbox = Mailbox { sender, capacity };
        let mut receive = pin!(receiver.recv());
        assert!(futures::poll!(receive.as_mut()).is_pending());
        for code in 1..=capacity {
            let mailbox = mailbox.clone();
            operations::spawn_task(async move {
                mailbox.publish(Event::Timer(Err(Errno::from_raw_os_error(code as i32))));
            })
            .await
            .unwrap();
        }
        assert_eq!(mailbox.sender.len(), capacity);
        let Event::Timer(Err(first)) = receive.await.unwrap() else {
            panic!("first completion")
        };
        assert_eq!(first.raw_os_error(), 1);
        for code in 2..=capacity {
            let Some(Event::Timer(Err(error))) = receiver.try_recv().unwrap() else {
                panic!("queued completion")
            };
            assert_eq!(error.raw_os_error(), code as i32);
        }
        assert!(receiver.try_recv().unwrap().is_none());
        assert_eq!(mailbox.sender.storage_bytes(), storage);
        mailbox.publish(Event::Timer(Ok(())));
        mailbox.sender.close();
        assert!(matches!(
            receiver.recv().await.unwrap(),
            Event::Timer(Ok(()))
        ));
        assert!(receiver.recv().await.is_err());
    }
}

#[test]
fn mailbox_bound_rejects_an_unreserved_completion_before_growth() {
    let (sender, _receiver) = kimojio::async_channel_unbounded_with_capacity(1);
    let mailbox = Mailbox {
        sender,
        capacity: 1,
    };
    mailbox.publish(Event::Timer(Ok(())));
    let storage = mailbox.sender.storage_bytes();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        mailbox.publish(Event::Timer(Ok(())));
    }));
    assert!(result.is_err());
    assert_eq!(mailbox.sender.len(), 1);
    assert_eq!(mailbox.sender.storage_bytes(), storage);
}

#[cfg(feature = "virtual-clock")]
#[kimojio::test]
async fn root_clock_and_native_timer_share_virtual_time() {
    operations::virtual_clock_enable(true);
    operations::virtual_clock_advance(Duration::from_secs(100));
    let origin = kimojio::clock_now();
    assert_eq!(clock(origin), Tick(0));
    let (sender, receiver) = kimojio::async_channel_unbounded_with_capacity(1);
    let mailbox = Mailbox {
        sender,
        capacity: 1,
    };
    let cancellation = Rc::new(CancellationToken::new());
    let at = add_duration(Tick(0), Duration::from_secs(30));
    let mut timer = pin!(timer(origin, at, cancellation, mailbox));
    assert!(operations::poll_once(timer.as_mut()).await.is_none());
    operations::virtual_clock_advance(Duration::from_secs(29));
    assert_eq!(clock(origin), Tick(29_000_000_000));
    assert!(operations::poll_once(timer.as_mut()).await.is_none());
    operations::virtual_clock_advance(Duration::from_secs(1));
    timer.await;
    assert_eq!(clock(origin), at);
    assert!(matches!(
        receiver.recv().await.unwrap(),
        Event::Timer(Ok(()))
    ));
}

#[cfg(feature = "virtual-clock")]
#[kimojio::test]
async fn virtual_root_shutdown_settles_accept_and_timer() {
    operations::virtual_clock_enable(true);
    operations::virtual_clock_advance(Duration::from_secs(100));
    operations::virtual_clock_set_idle_advance(|now, next| {
        next.map(|at| at.saturating_duration_since(now))
    });
    let origin = kimojio::clock_now();
    let config = Config {
        run_for: Duration::from_secs(30),
        ..Config::default()
    };
    let limit = config.chat.hub.max_total_bytes;
    let stats = run(config).await.unwrap();
    assert_eq!(stats.resident_clients, 0);
    assert_eq!(stats.active_clients, 0);
    assert!(stats.peak_bytes <= limit);
    assert!(clock(origin) >= Tick(30_000_000_000));
}
