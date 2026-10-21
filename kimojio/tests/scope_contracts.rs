//! Shared public-API contracts: install as kimojio/tests/scope_contracts.rs.
//! Native socket I/O only; no sleeps, virtual clock, tracing tags, or registry assumptions.
#![allow(clippy::async_yields_async, reason = "Returning an original future across scope exit is the settlement contract under test.")]
#![allow(clippy::await_holding_refcell_ref, reason = "Shared futures are polled serially; callbacks do not access a RefCell while finish holds its borrow.")]
use futures::{Future, StreamExt, future::LocalBoxFuture, stream::FuturesUnordered};
use kimojio::{AsyncEvent, CanceledError, Errno, operations as op};
use std::{
    cell::{Cell, RefCell},
    os::unix::net::UnixStream,
    pin::Pin,
    rc::Rc,
    sync::{Arc, atomic::{AtomicUsize, Ordering::SeqCst}},
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    thread::{ThreadId, current},
};

fn poll<F: Future + Unpin>(f: &mut F) -> Poll<F::Output> {
    Pin::new(f).poll(&mut Context::from_waker(futures::task::noop_waker_ref()))
}

// Drive kernel completions without relying on a notification from a manually polled future.
// Exhaustion is a failure, never a substitute result. Borrowed futures are dropped by callers.
async fn finish<F: Future>(f: F) -> F::Output {
    let mut f = Box::pin(f);
    for _ in 0..16_384 {
        if let Poll::Ready(value) = poll(&mut f) { return value; }
        op::yield_io().await;
    }
    panic!("contract future did not settle within the polling budget");
}

type Shared<T> = Rc<RefCell<LocalBoxFuture<'static, T>>>;
thread_local! {
    static ON_WAKE: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
}

// Arc makes every RawWaker ownership operation thread safe. The non-Send proof callback
// lives in TLS, not in the waker. Tests invoke it synchronously inside an active runtime
// task; foreign-thread wakes are explicitly unsupported and never exercised here.
struct Probe { hits: AtomicUsize, owner: ThreadId, callback: bool }
impl Probe {
    fn new(callback: bool) -> Arc<Self> {
        Arc::new(Self { hits: AtomicUsize::new(0), owner: current().id(), callback })
    }
    fn record(&self) {
        assert_eq!(self.owner, current().id(), "proof wake must remain task-local");
        self.hits.fetch_add(1, SeqCst);
        if self.callback {
            let callback = ON_WAKE.with(|slot| slot.borrow_mut().take());
            if let Some(callback) = callback { callback(); }
        }
    }
    fn waker(self: &Arc<Self>) -> Waker {
        // SAFETY: each raw pointer owns one Arc; clone/wake/drop balance that ownership.
        unsafe { Waker::from_raw(RawWaker::new(Arc::into_raw(self.clone()).cast(), &VTABLE)) }
    }
}
static VTABLE: RawWakerVTable = RawWakerVTable::new(
    |p| { unsafe { Arc::increment_strong_count(p.cast::<Probe>()) }; RawWaker::new(p, &VTABLE) },
    |p| { let probe = unsafe { Arc::from_raw(p.cast::<Probe>()) }; probe.record(); },
    |p| unsafe { &*p.cast::<Probe>() }.record(),
    |p| drop(unsafe { Arc::from_raw(p.cast::<Probe>()) }),
);
fn counted<F: Future>(f: Pin<&mut F>, probe: &Arc<Probe>) -> Poll<F::Output> {
    f.poll(&mut Context::from_waker(&probe.waker()))
}

#[kimojio::test]
async fn retained_native_read_is_ready_on_first_poll_after_scope_exit() {
    let (reader, _keep_peer_open) = UnixStream::pair().unwrap();
    let mut bytes = [0; 1];
    let mut read = op::io_scope(async || {
        let mut read = Box::pin(op::read(&reader, &mut bytes));
        assert!(poll(&mut read).is_pending());
        read
    }).await;
    assert_eq!(poll(&mut read), Poll::Ready(Err(Errno::CANCELED)));
    drop(read); // release the borrowed buffer before inspecting/reusing it
    assert_eq!(finish(op::nop()).await, Ok(()));
}

#[kimojio::test]
async fn exiting_scope_does_not_cancel_external_sibling_scope_io() {
    let (a, _a_peer) = UnixStream::pair().unwrap();
    let (b, b_peer) = UnixStream::pair().unwrap();
    let release = AsyncEvent::new();
    let mut siblings: FuturesUnordered<LocalBoxFuture<'_, (u8, Result<usize, Errno>)>> =
        FuturesUnordered::new();
    siblings.push(Box::pin(async {
        let mut bytes = [0];
        let mut read = op::io_scope(async || {
            let mut read = Box::pin(op::read(&a, &mut bytes));
            assert!(poll(&mut read).is_pending());
            release.wait().await.unwrap();
            read
        }).await;
        (0, match poll(&mut read) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("exiting scope did not settle its read"),
        })
    }));
    siblings.push(Box::pin(op::io_scope(async || {
        let mut bytes = [0];
        let n = op::read(&b, &mut bytes).await;
        assert_eq!(bytes, [b'B']);
        (1, n)
    })));
    assert!(poll(&mut siblings.next()).is_pending()); // both siblings started
    release.set();
    assert_eq!(finish(siblings.next()).await, Some((0, Err(Errno::CANCELED))));
    assert_eq!(finish(op::write(&b_peer, b"B")).await, Ok(1));
    assert_eq!(finish(siblings.next()).await, Some((1, Ok(1))));
    assert!(siblings.is_empty());
}

#[kimojio::test]
async fn same_wait_in_multiple_scopes_cancellation_then_retry_is_isolated() {
    for _ in 0..16 {
        let event = AsyncEvent::new();
        let wait = RefCell::new(Box::pin(event.wait()));
        let exit_a = Cell::new(false);
        let cancel_b = Cell::new(false);
        let mut a = Box::pin(op::io_scope(async || {
            futures::future::poll_fn(|_| {
                if exit_a.get() { return Poll::Ready(()); }
                assert!(poll(&mut *wait.borrow_mut()).is_pending());
                Poll::Pending
            }).await;
        }));
        let mut b = Box::pin(op::io_scope(async || {
            futures::future::poll_fn(|_| {
                if cancel_b.get() { op::io_scope_cancel(); return Poll::Ready(()); }
                assert!(poll(&mut *wait.borrow_mut()).is_pending());
                Poll::Pending
            }).await;
        }));
        assert!(poll(&mut a).is_pending());
        assert!(poll(&mut b).is_pending()); // SAME future, not two waits on one event
        cancel_b.set(true);
        assert_eq!(poll(&mut b), Poll::Ready(()));
        assert_eq!(poll(&mut *wait.borrow_mut()), Poll::Ready(Err(CanceledError {})));
        assert!(poll(&mut *wait.borrow_mut()).is_pending()); // retry outside either scope
        exit_a.set(true);
        assert_eq!(poll(&mut a), Poll::Ready(())); // old registration must not poison retry
        assert!(poll(&mut *wait.borrow_mut()).is_pending());
        event.set();
        assert_eq!(poll(&mut *wait.borrow_mut()), Poll::Ready(Ok(())));
    }
}

#[kimojio::test]
async fn canceled_wait_passes_available_notification_before_future_drop() {
    let event = AsyncEvent::new();
    let first_probe = Probe::new(false);
    let second_probe = Probe::new(false);
    let mut first = Box::pin(event.wait());
    op::io_scope(async || {
        assert!(counted(first.as_mut(), &first_probe).is_pending());
        op::io_scope_cancel();
    }).await;
    let mut second = Box::pin(event.wait());
    assert!(counted(second.as_mut(), &second_probe).is_pending());
    first_probe.hits.store(0, SeqCst);
    event.set_wake_one();
    // Do not assume FIFO: whichever waiter received the notification, the live waiter
    // must have a wake after cancellation is observed, while `first` is still retained.
    // Cancellation may already have consumed the canceled waiter's stored waker.
    // The contract is notification handoff when cancellation is observed, not
    // whether this already-canceled waiter gets another wake beforehand.
    assert!(first_probe.hits.load(SeqCst) + second_probe.hits.load(SeqCst) <= 1);
    assert_eq!(counted(first.as_mut(), &first_probe), Poll::Ready(Err(CanceledError {})));
    assert!(second_probe.hits.load(SeqCst) > 0, "notification was stranded on canceled wait");
    assert_eq!(counted(second.as_mut(), &second_probe), Poll::Ready(Ok(())));
    drop(first);
}

#[kimojio::test]
async fn repeated_pending_polling_and_wait_drop_leave_runtime_usable() {
    let event = AsyncEvent::new();
    for _ in 0..16 {
        op::io_scope(async || {
            for _ in 0..8 {
                let mut repeated = Box::pin(event.wait());
                for _ in 0..32 { assert!(poll(&mut repeated).is_pending()); }
                drop(repeated);
                let mut waits = FuturesUnordered::new();
                for _ in 0..32 { waits.push(event.wait()); }
                for _ in 0..8 { assert!(poll(&mut waits.next()).is_pending()); }
                drop(waits); // all borrowed waits dropped before cancellation
            }
            op::io_scope_cancel();
            let mut live = Box::pin(event.wait());
            assert!(poll(&mut live).is_pending());
            event.set_wake_one();
            assert_eq!(poll(&mut live), Poll::Ready(Ok(())));
            event.reset();
            assert_eq!(finish(op::nop()).await, Ok(()));
        }).await;
    }
}

#[kimojio::test]
async fn native_positive_write_then_canceled_continuation_preserves_byte_count() {
    let written = Cell::new(0);
    let mut peers = Vec::new();
    let mut work = FuturesUnordered::new();
    op::io_scope(async || {
        for _ in 0..16 {
            let (socket, peer) = UnixStream::pair().unwrap();
            peers.push(peer);
            let written = &written;
            work.push(async move {
                let n = op::write(&socket, b"payload").await.unwrap();
                assert_eq!(n, 7);
                written.set(written.get() + n);
                let mut continuation = [0];
                (n, op::read(&socket, &mut continuation).await)
            });
        }
        assert!(poll(&mut work.next()).is_pending());
        for peer in &peers {
            let mut bytes = [0; 7];
            assert_eq!(finish(op::read(peer, &mut bytes)).await, Ok(7));
            assert_eq!(&bytes, b"payload");
        }
        for _ in 0..16_384 {
            assert!(poll(&mut work.next()).is_pending());
            if written.get() == 16 * 7 { break; }
            op::yield_io().await;
        }
        assert_eq!(written.get(), 16 * 7, "positive native writes were not observed");
        op::io_scope_cancel();
        let mut accounted = 0;
        while let Some((n, continuation)) = finish(work.next()).await {
            assert_eq!(continuation, Err(Errno::CANCELED));
            accounted += n;
        }
        assert_eq!(accounted, written.get());
    }).await;
}

#[kimojio::test]
async fn reentrant_explicit_cancel_uses_a_snapshot() { reentrant_snapshot(false).await; }
#[kimojio::test]
async fn reentrant_scope_exit_uses_a_snapshot() { reentrant_snapshot(true).await; }

async fn reentrant_snapshot(implicit_exit: bool) {
    for round in 0..8 {
        let trigger = AsyncEvent::new();
        let event = Rc::new(AsyncEvent::new());
        let (reader, writer) = UnixStream::pair().unwrap();
        let wait: Shared<Result<(), CanceledError>> = Rc::new(RefCell::new(Box::pin({
            let event = event.clone();
            async move { event.wait().await }
        })));
        let read: Shared<Result<usize, Errno>> = Rc::new(RefCell::new(Box::pin(async move {
            let mut byte = [0];
            let n = op::read(&reader, &mut byte).await?;
            assert_eq!(byte, [b'N']);
            Ok(n)
        })));
        let active = Rc::new(Cell::new(false));
        ON_WAKE.with(|slot| {
            let (wait, read, active) = (wait.clone(), read.clone(), active.clone());
            assert!(slot.borrow_mut().replace(Box::new(move || {
                assert!(active.get(), "callback must run inside active cancellation task");
                assert!(poll(&mut *wait.borrow_mut()).is_pending());
                assert!(poll(&mut *read.borrow_mut()).is_pending());
            })).is_none());
        });
        let probe = Probe::new(true);
        let mut old_wait = Box::pin(trigger.wait());
        op::io_scope(async || {
            active.set(true);
            if implicit_exit {
                op::io_scope(async || {
                    assert!(counted(old_wait.as_mut(), &probe).is_pending());
                }).await;
            } else {
                assert!(counted(old_wait.as_mut(), &probe).is_pending());
                op::io_scope_cancel();
            }
            active.set(false);
            assert!(probe.hits.load(SeqCst) > 0);
            assert!(ON_WAKE.with(|slot| slot.borrow().is_none()));
            assert_eq!(poll(&mut old_wait), Poll::Ready(Err(CanceledError {})));
            assert!(poll(&mut *wait.borrow_mut()).is_pending(), "new wait entered old snapshot");
            if round % 2 == 0 {
                assert_eq!(finish(op::write(&writer, b"N")).await, Ok(1));
                assert_eq!(finish(read.borrow_mut().as_mut()).await, Ok(1));
                event.set();
                assert_eq!(poll(&mut *wait.borrow_mut()), Poll::Ready(Ok(())));
            } else {
                op::io_scope_cancel(); // new registrations must remain tracked for NEXT snapshot
                assert_eq!(finish(read.borrow_mut().as_mut()).await, Err(Errno::CANCELED));
                assert_eq!(poll(&mut *wait.borrow_mut()), Poll::Ready(Err(CanceledError {})));
            }
        }).await;
    }
}
