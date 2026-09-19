//! Direct io_uring executor. Neither HTTP nor application policy lives here.
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rustix_uring::{
    Errno, IoUring, opcode,
    squeue::Entry,
    types::{SubmitArgs, Timespec},
};

const CANCEL_BIT: u64 = 1 << 63;

/// A pinned-in-place kernel operation, owned until its original CQE.
///
/// # Safety
/// `entry` must reference only live descriptors and storage owned by this
/// operation, or storage whose owner is retained by it. Writable regions must
/// not alias concurrent operations. Moving the containing Box must not
/// invalidate any pointer. `completed` must settle newly produced resources
/// (notably accepted/opened descriptors) even when the caller drops the event.
/// Each entry must produce exactly one original CQE: multishot operations and
/// zero-copy notification CQEs are not supported. Neither method may panic.
pub unsafe trait Operation {
    /// # Safety
    /// Call exactly once, in stable storage, immediately before submission.
    /// Retain the operation until its original CQE.
    unsafe fn entry(&mut self) -> Entry;
    /// # Safety
    /// Supply this operation's authentic final CQE result exactly once.
    /// The kernel must no longer access its storage.
    unsafe fn completed(&mut self, result: Result<u32, Errno>);
    /// Close operations must finish rather than lose a transferred descriptor
    /// to cancellation before the close syscall executes.
    fn cancelable(&self) -> bool {
        true
    }
}

pub enum Event<O> {
    Completed {
        token: u64,
        operation: Box<O>,
        result: Result<u32, Errno>,
    },
    /// This is separate from the original operation's completion.
    Canceled {
        token: u64,
        result: Result<u32, Errno>,
    },
}

struct Pending<O> {
    operation: Box<O>,
    cancellation: Cancellation,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Cancellation {
    None,
    Deferred,
    Submitted,
}

impl<O> Pending<O> {
    fn request_cancel(&mut self) -> bool {
        if self.cancellation != Cancellation::None {
            return false;
        }
        self.cancellation = Cancellation::Deferred;
        true
    }
}

struct CancellationBudget {
    limit: usize,
    outstanding: BTreeSet<u64>,
}

impl CancellationBudget {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            outstanding: BTreeSet::new(),
        }
    }

    fn next<O>(&mut self, pending: &mut BTreeMap<u64, Pending<O>>) -> Option<u64> {
        if self.outstanding.len() == self.limit {
            return None;
        }
        let (&token, pending) = pending
            .iter_mut()
            .find(|(_, pending)| pending.cancellation == Cancellation::Deferred)?;
        pending.cancellation = Cancellation::Submitted;
        assert!(self.outstanding.insert(token));
        Some(token)
    }

    fn acknowledge(&mut self, token: u64) -> bool {
        self.outstanding.remove(&token)
    }
}

pub struct Ring<O: Operation> {
    ring: IoUring,
    pending: BTreeMap<u64, Pending<O>>,
    next: u64,
    capacity: usize,
    cancellations: CancellationBudget,
    deferred_cancellations: usize,
}

impl<O: Operation> Ring<O> {
    pub fn new(capacity: u32) -> Result<Self, Errno> {
        Self::with_cancel_capacity(capacity, capacity)
    }

    /// Original operations and cancellation requests have separate limits.
    /// A cancellation slot remains occupied until its own CQE, even when the
    /// original operation already completed. Deferred requests stay in the
    /// bounded original-operation slots, not in another queue.
    pub fn with_cancel_capacity(capacity: u32, cancel_capacity: u32) -> Result<Self, Errno> {
        if cancel_capacity == 0 || cancel_capacity > capacity {
            return Err(Errno::INVAL);
        }
        Ok(Self {
            ring: IoUring::new(capacity)?,
            pending: BTreeMap::new(),
            next: 1,
            capacity: capacity as usize,
            cancellations: CancellationBudget::new(cancel_capacity as usize),
            deferred_cancellations: 0,
        })
    }

    pub fn available(&self) -> usize {
        self.capacity - self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.cancellations.outstanding.is_empty()
    }

    /// On capacity rejection no operation is submitted, and ownership returns.
    pub fn submit(&mut self, operation: O) -> Result<u64, O> {
        if self.available() == 0 || self.next == CANCEL_BIT {
            return Err(operation);
        }
        let token = self.next;
        self.next += 1;
        let mut operation = Box::new(operation);
        // The Box remains in pending until the authentic original CQE.
        let entry = unsafe { operation.entry() }.user_data(token);
        self.pending.insert(
            token,
            Pending {
                operation,
                cancellation: Cancellation::None,
            },
        );
        self.push(entry);
        Ok(token)
    }

    fn push(&mut self, entry: Entry) {
        loop {
            // All entry pointers are backed by a stable pending operation.
            if unsafe { self.ring.submission().push(&entry) }.is_ok() {
                break;
            }
            if let Err(error) = self.ring.submit()
                && error != Errno::INTR
                && error != Errno::AGAIN
                && error != Errno::BUSY
            {
                // Unrecoverable submission errors cannot free kernel-borrowed
                // allocations. Abort is safer than unwinding across them.
                eprintln!("fatal io_uring submission error: {error}");
                std::process::abort();
            }
        }
    }

    /// Accepts one cancellation request per original operation. Acceptance can
    /// defer submission until a cancellation CQE releases a budget slot.
    pub fn cancel(&mut self, token: u64) -> bool {
        let Some(pending) = self.pending.get_mut(&token) else {
            return false;
        };
        if !pending.operation.cancelable() || !pending.request_cancel() {
            return false;
        }
        self.deferred_cancellations += 1;
        self.submit_cancellations();
        true
    }

    fn submit_cancellations(&mut self) {
        while self.deferred_cancellations != 0 {
            let Some(token) = self.cancellations.next(&mut self.pending) else {
                break;
            };
            self.deferred_cancellations -= 1;
            self.push(
                opcode::AsyncCancel::new(token.into())
                    .build()
                    .user_data(token | CANCEL_BIT),
            );
        }
    }

    /// Waits only in the root executor. A timeout returns no event; its caller
    /// supplies the resulting monotonic observation to the protocol FSM.
    pub fn poll(&mut self, wait: Option<Duration>) -> Result<Vec<Event<O>>, Errno> {
        let ready = !self.ring.completion().is_empty();
        let count = usize::from(!ready && wait != Some(Duration::ZERO) && !self.is_empty());
        let result = match wait {
            Some(duration) if count > 0 => {
                let timeout = Timespec::from(duration);
                self.ring
                    .submitter()
                    .submit_with_args(count, &SubmitArgs::new().timespec(&timeout))
            }
            _ => self.ring.submit_and_wait(count),
        };
        match result {
            Ok(_) | Err(Errno::INTR | Errno::AGAIN | Errno::BUSY | Errno::TIME) => {}
            Err(error) => return Err(error),
        }
        let mut events = Vec::new();
        for cqe in self.ring.completion() {
            let token = cqe.user_data().u64_();
            let result = cqe.result();
            if token & CANCEL_BIT != 0 {
                let token = token & !CANCEL_BIT;
                assert!(
                    self.cancellations.acknowledge(token),
                    "unknown cancellation CQE token"
                );
                events.push(Event::Canceled { token, result });
            } else {
                let mut pending = self.pending.remove(&token).expect("unknown CQE token");
                if pending.cancellation == Cancellation::Deferred {
                    self.deferred_cancellations -= 1;
                }
                // This token names the original, non-multishot operation.
                unsafe { pending.operation.completed(result) };
                events.push(Event::Completed {
                    token,
                    operation: pending.operation,
                    result,
                });
            }
        }
        // Original CQEs in this batch remove their deferred requests first.
        // A cancellation is unnecessary if its original already settled.
        self.submit_cancellations();
        Ok(events)
    }
}

impl<O: Operation> Drop for Ring<O> {
    fn drop(&mut self) {
        let tokens: Vec<_> = self.pending.keys().copied().collect();
        for token in tokens {
            self.cancel(token);
        }
        while !self.is_empty() {
            if let Err(error) = self.poll(None) {
                eprintln!("fatal io_uring settlement error: {error}");
                std::process::abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn originals() -> BTreeMap<u64, Pending<()>> {
        [1, 2]
            .into_iter()
            .map(|token| {
                let mut pending = Pending {
                    operation: Box::new(()),
                    cancellation: Cancellation::None,
                };
                assert!(pending.request_cancel());
                (token, pending)
            })
            .collect()
    }

    #[test]
    fn original_before_ack_does_not_release_cancellation_capacity() {
        let mut pending = originals();
        let mut budget = CancellationBudget::new(1);
        assert_eq!(budget.next(&mut pending), Some(1));
        pending.remove(&1);
        assert_eq!(budget.next(&mut pending), None);
        assert_eq!(budget.outstanding.len(), 1);
        assert!(budget.acknowledge(1));
        assert_eq!(budget.next(&mut pending), Some(2));
        assert_eq!(budget.outstanding.len(), 1);
    }

    #[test]
    fn ack_before_original_preserves_original_and_deduplicates_cancel() {
        let mut pending = originals();
        let mut budget = CancellationBudget::new(1);
        assert_eq!(budget.next(&mut pending), Some(1));
        assert!(budget.acknowledge(1));
        assert!(pending.contains_key(&1));
        assert!(!pending.get_mut(&1).unwrap().request_cancel());
        assert_eq!(budget.next(&mut pending), Some(2));
        assert!(budget.acknowledge(2));
        assert!(!budget.acknowledge(2));
        assert_eq!(budget.next(&mut pending), None);
        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn completing_deferred_original_discards_only_its_deferred_request() {
        let mut pending = originals();
        let mut budget = CancellationBudget::new(1);
        assert_eq!(budget.next(&mut pending), Some(1));
        assert!(!pending.get_mut(&2).unwrap().request_cancel());
        pending.remove(&2);
        assert_eq!(budget.outstanding.len(), 1);
        assert!(budget.acknowledge(1));
        assert_eq!(budget.next(&mut pending), None);
        assert!(budget.outstanding.is_empty());
    }
}
