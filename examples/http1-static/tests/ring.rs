use http1_static::ring::{Event, Operation, Ring};
use rustix::fd::{AsRawFd, OwnedFd};
use rustix::net::{self, AddressFamily, SocketFlags, SocketType};
use rustix_uring::{Errno, opcode, squeue::Entry, types::Fd};
use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

struct Receive {
    socket: Rc<OwnedFd>,
    buffer: Box<[u8; 8]>,
    completed: bool,
}

// The descriptor and buffer are owned by the operation and survive its CQE.
unsafe impl Operation for Receive {
    unsafe fn entry(&mut self) -> Entry {
        opcode::Recv::new(Fd(self.socket.as_raw_fd()), self.buffer.as_mut_ptr(), 8).build()
    }
    unsafe fn completed(&mut self, _: Result<u32, Errno>) {
        self.completed = true;
    }
}

#[test]
fn real_pending_receive_cancellation_has_two_completions() {
    let (left, _right) = net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    let mut ring = Ring::new(8).expect("real io_uring required");
    let token = ring
        .submit(Receive {
            socket: Rc::new(left),
            buffer: Box::new([0; 8]),
            completed: false,
        })
        .ok()
        .unwrap();
    assert!(ring.poll(Some(Duration::ZERO)).unwrap().is_empty());
    assert!(ring.cancel(token));
    assert!(!ring.cancel(token));
    let mut original = false;
    let mut cancellation = false;
    while !ring.is_empty() {
        for event in ring.poll(Some(Duration::from_secs(1))).unwrap() {
            match event {
                Event::Completed {
                    token: id,
                    operation,
                    result,
                } => {
                    assert_eq!(id, token);
                    assert!(operation.completed);
                    assert_eq!(result, Err(Errno::CANCELED));
                    original = true;
                }
                Event::Canceled { token: id, result } => {
                    assert_eq!(id, token);
                    assert_eq!(result, Ok(0));
                    cancellation = true;
                }
            }
        }
    }
    assert!(original && cancellation);
}

#[test]
fn drop_cancels_and_drains_before_releasing_resources() {
    let (left, _right) = net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    let socket = Rc::new(left);
    let mut ring = Ring::new(8).expect("real io_uring required");
    ring.submit(Receive {
        socket: socket.clone(),
        buffer: Box::new([0; 8]),
        completed: false,
    })
    .ok()
    .unwrap();
    ring.poll(Some(Duration::ZERO)).unwrap();
    assert_eq!(Rc::strong_count(&socket), 2);
    drop(ring);
    assert_eq!(Rc::strong_count(&socket), 1);
}

#[test]
fn real_cancellations_defer_and_progress_with_one_acknowledgement_slot() {
    let (left, _right) = net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    let socket = Rc::new(left);
    let mut ring = Ring::with_cancel_capacity(8, 1).expect("real io_uring required");
    let mut tokens = BTreeSet::new();
    for _ in 0..3 {
        tokens.insert(
            ring.submit(Receive {
                socket: socket.clone(),
                buffer: Box::new([0; 8]),
                completed: false,
            })
            .ok()
            .unwrap(),
        );
    }
    assert!(ring.poll(Some(Duration::ZERO)).unwrap().is_empty());
    for &token in &tokens {
        assert!(ring.cancel(token));
        assert!(!ring.cancel(token));
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut originals = BTreeSet::new();
    let mut acknowledgements = BTreeSet::new();
    while !ring.is_empty() {
        assert!(Instant::now() < deadline, "deferred cancellation stalled");
        for event in ring.poll(Some(Duration::from_millis(100))).unwrap() {
            match event {
                Event::Completed {
                    token,
                    operation,
                    result,
                } => {
                    assert!(operation.completed);
                    assert_eq!(result, Err(Errno::CANCELED));
                    assert!(originals.insert(token));
                }
                Event::Canceled { token, result } => {
                    assert_eq!(result, Ok(0));
                    assert!(acknowledgements.insert(token));
                }
            }
        }
    }
    assert_eq!(originals, tokens);
    assert_eq!(acknowledgements, tokens);
    assert_eq!(Rc::strong_count(&socket), 1);
}

#[test]
fn drop_drains_more_originals_than_cancellation_slots() {
    let (left, _right) = net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    let socket = Rc::new(left);
    let mut ring = Ring::with_cancel_capacity(8, 1).expect("real io_uring required");
    for _ in 0..3 {
        ring.submit(Receive {
            socket: socket.clone(),
            buffer: Box::new([0; 8]),
            completed: false,
        })
        .ok()
        .unwrap();
    }
    assert!(ring.poll(Some(Duration::ZERO)).unwrap().is_empty());
    assert_eq!(Rc::strong_count(&socket), 4);
    drop(ring);
    assert_eq!(Rc::strong_count(&socket), 1);
}

#[test]
fn cancellation_capacity_must_be_nonzero_and_fit_the_ring() {
    assert_eq!(
        Ring::<Receive>::with_cancel_capacity(8, 0).err(),
        Some(Errno::INVAL)
    );
    assert_eq!(
        Ring::<Receive>::with_cancel_capacity(8, 9).err(),
        Some(Errno::INVAL)
    );
}
