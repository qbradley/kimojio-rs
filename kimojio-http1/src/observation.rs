use std::{cell::Cell, fmt, rc::Rc};

#[cfg(feature = "metrics")]
use std::cell::RefCell;

#[cfg(feature = "metrics")]
use futures::FutureExt;
#[cfg(feature = "metrics")]
use kimojio::{CancellationToken, Receiver, Sender, SenderOneshot, async_channel, oneshot};

use crate::Error;
#[cfg(feature = "metrics")]
use crate::MetricsSnapshot;
use crate::{ConnectionId, LogEvent, Tick};

#[cfg(feature = "metrics")]
pub(crate) type SnapshotRequest = SenderOneshot<Result<MetricsSnapshot, Error>>;

type Logger = Box<dyn Fn(ConnectionId, Tick, LogEvent)>;

struct Inner {
    bound: Cell<bool>,
    closed: Cell<bool>,
    #[cfg(feature = "metrics")]
    requests: Sender<SnapshotRequest>,
    #[cfg(feature = "metrics")]
    receiver: RefCell<Option<Receiver<SnapshotRequest>>>,
    #[cfg(feature = "metrics")]
    final_snapshot: Cell<Option<MetricsSnapshot>>,
    #[cfg(feature = "metrics")]
    stopped: CancellationToken,
    logger: Option<Logger>,
}

/// Opt-in observations for one connection, shared on the driver's local thread.
///
/// Attach this handle through [`crate::Config::observation`]. A handle cannot be
/// attached again, even after its first connection closes.
#[derive(Clone)]
pub struct Observation(Rc<Inner>);

impl fmt::Debug for Observation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Observation")
            .field("bound", &self.0.bound.get())
            .field("closed", &self.0.closed.get())
            .finish_non_exhaustive()
    }
}

impl Default for Observation {
    fn default() -> Self {
        Self::new()
    }
}

impl Observation {
    /// Creates a handle without a diagnostic logger.
    pub fn new() -> Self {
        Self::create(None)
    }

    /// Calls `logger` synchronously at each core diagnostic boundary.
    ///
    /// The callback must not block or panic. Use caller-owned `Cell` or
    /// `RefCell` storage if it needs mutable state. Events are never queued.
    pub fn with_logger(logger: impl Fn(ConnectionId, Tick, LogEvent) + 'static) -> Self {
        Self::create(Some(Box::new(logger)))
    }

    fn create(logger: Option<Logger>) -> Self {
        #[cfg(feature = "metrics")]
        let (requests, receiver) = async_channel();
        Self(Rc::new(Inner {
            bound: Cell::new(false),
            closed: Cell::new(false),
            #[cfg(feature = "metrics")]
            requests,
            #[cfg(feature = "metrics")]
            receiver: RefCell::new(Some(receiver)),
            #[cfg(feature = "metrics")]
            final_snapshot: Cell::new(None),
            #[cfg(feature = "metrics")]
            stopped: CancellationToken::new(),
            logger,
        }))
    }

    /// Requests one coherent snapshot from the driver's fair input loop.
    ///
    /// Keep the driver polled concurrently. Requests use a bounded channel.
    /// After terminal close, this returns the cached final snapshot. Startup
    /// failure or driver cancellation without terminal close returns `Closed`.
    #[cfg(feature = "metrics")]
    pub async fn snapshot(&self) -> Result<MetricsSnapshot, Error> {
        if self.0.closed.get() || self.final_snapshot().is_some() {
            return self.final_result();
        }
        let query = async {
            let (reply, response) = oneshot();
            if self.0.requests.send(reply).await.is_err() {
                return self.final_result();
            }
            response
                .recv()
                .await
                .unwrap_or_else(|_| self.final_result())
        }
        .fuse();
        let stopped = self.0.stopped.cancelled().fuse();
        futures::pin_mut!(query, stopped);
        futures::select_biased! {
            _ = stopped => self.final_result(),
            result = query => result,
        }
    }

    /// Returns a snapshot only after the core's terminal `closed` callback.
    #[cfg(feature = "metrics")]
    pub fn final_snapshot(&self) -> Option<MetricsSnapshot> {
        self.0.final_snapshot.get()
    }

    #[cfg(feature = "metrics")]
    fn final_result(&self) -> Result<MetricsSnapshot, Error> {
        self.final_snapshot().ok_or(Error::Closed)
    }

    pub(crate) fn bind(&self) -> Result<BoundObservation, Error> {
        if self.0.bound.replace(true) {
            return Err(Error::ObservationInUse);
        }
        Ok(BoundObservation {
            observation: self.clone(),
            #[cfg(feature = "metrics")]
            requests: self.0.receiver.borrow_mut().take().expect("first binding"),
        })
    }

    pub(crate) fn log(&self, connection: ConnectionId, now: Tick, event: LogEvent) {
        if let Some(logger) = &self.0.logger {
            logger(connection, now, event);
        }
    }
}

pub(crate) struct BoundObservation {
    pub observation: Observation,
    #[cfg(feature = "metrics")]
    pub requests: Receiver<SnapshotRequest>,
}

impl BoundObservation {
    #[cfg(feature = "metrics")]
    pub fn finish(&self, snapshot: MetricsSnapshot) {
        self.observation.0.final_snapshot.set(Some(snapshot));
    }
}

impl Drop for BoundObservation {
    fn drop(&mut self) {
        self.observation.0.closed.set(true);
        // Channel closure does not release a sender waiting for a full slot to reset.
        #[cfg(feature = "metrics")]
        self.observation.0.stopped.cancel();
        // Dropping the receiver closes the channel but can retain its queued reply.
        #[cfg(feature = "metrics")]
        while let Ok(Some(reply)) = self.requests.try_recv() {
            let _ = reply.send(self.observation.final_result());
        }
    }
}
