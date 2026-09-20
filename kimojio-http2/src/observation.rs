use http::Response;
use kimojio_fsm_http2::{ReceiveEnd, SendStop, StreamId, StreamOutcome};

use crate::Error;

/// The first failed DATA-buffer receipt, separate from stream retirement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendFailure {
    /// Known accepted payload bytes from this buffer, not the total upload.
    pub accepted: usize,
    /// False means that further transport acceptance is unknown.
    pub exact: bool,
    pub reason: SendStop,
}

/// Authoritative core retirement plus independent receive and wrapper diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamReport {
    pub stream: StreamId,
    /// Copied directly from the core's retirement event, never inferred from I/O.
    pub outcome: StreamOutcome,
    /// The core's receive-half notification, if one was delivered.
    pub receive_outcome: Option<StreamOutcome>,
    pub send_failure: Option<SendFailure>,
    /// Context used by the conventional `IncomingBody::completion` result.
    ///
    /// This can be a producer, callback, cancellation, or send-receipt error.
    /// It does not replace `outcome`.
    pub error: Option<Error>,
}

impl StreamReport {
    pub(crate) fn completion_result(&self) -> Result<StreamOutcome, Error> {
        match &self.error {
            Some(error) => Err(error.clone()),
            None if self.outcome == StreamOutcome::Complete => Ok(self.outcome),
            None => Err(Error::Stream(self.outcome)),
        }
    }
}

/// Optional client lifecycle observation without a per-request event queue.
///
/// Callbacks run synchronously on the driver and must not block. Implementations
/// can use application-owned channels to publish a separate retirement handle.
/// An unadmitted request has no stream ID and emits no retirement event.
/// Dropping the driver can also prevent retirement; no outcome is fabricated.
pub trait RequestObserver {
    /// The core committed request metadata and assigned this ID.
    fn admitted(&mut self, _stream: StreamId) {}
    /// An actual informational head arrived from the peer.
    fn informational(&mut self, _head: Response<()>) {}
    /// The receive half ended, independently of upload and lease settlement.
    fn receive_end(&mut self, _end: ReceiveEnd) {}
    /// The core retired the stream, even if sending failed before final headers.
    ///
    /// Clone the report to retain it beyond this callback.
    fn retired(&mut self, _report: &StreamReport) {}
}

pub(crate) type Observer = Box<dyn RequestObserver>;

pub(crate) fn notify(
    observer: &mut Option<Observer>,
    notification: impl FnOnce(&mut dyn RequestObserver),
) -> Result<(), Error> {
    if let Some(callback) = observer
        && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            notification(callback.as_mut());
        }))
        .is_err()
    {
        observer.take();
        return Err(Error::Application("request observer panicked".into()));
    }
    Ok(())
}
