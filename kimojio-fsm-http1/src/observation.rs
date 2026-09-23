//! Optional connection snapshots and content-free diagnostics.
//!
//! Enable the `metrics` feature for counters and gauges. Logging is available
//! without that feature and runs synchronously during a drive call.

use crate::*;

/// Coarse lifecycle information, not permission to issue a command.
#[cfg(feature = "metrics")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SnapshotPhase {
    /// Normal HTTP request/response processing.
    Http,
    /// Graceful shutdown is draining accepted work.
    Draining,
    /// An accepted protocol upgrade is transferring ownership.
    Upgrading,
    /// A bounded protocol error response is being written.
    ErrorResponse,
    /// Transport close is pending or in progress.
    Closing,
    /// The connection reached terminal close.
    Closed,
    /// The transport was transferred to another protocol.
    HandedOff,
}

/// Connection-lifetime counters. Invalid or rejected completions do not count.
#[cfg(feature = "metrics")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct Counters {
    /// Successfully settled transport read operations.
    pub read_completions: u64,
    /// Bytes accepted by successful reads.
    pub read_bytes: u64,
    /// Successfully settled transport write operations.
    pub write_completions: u64,
    /// Newly confirmed bytes, including framing. Unknown progress is excluded.
    pub written_bytes_lower_bound: u64,
    /// Write completions whose accepted byte count could not be determined.
    pub uncertain_write_completions: u64,
    /// Body leases offered to the application.
    pub body_deliveries: u64,
    /// Bytes offered through leases, including repeated offers after partial consumption.
    pub body_bytes_delivered: u64,
    /// Payload bytes the application reported as consumed from body leases.
    pub body_bytes_consumed: u64,
    /// Payload bytes accepted from outgoing body producers.
    pub producer_bytes_accepted: u64,
    /// Created exchange records, including records that subsequently fail head admission.
    pub exchanges_started: u64,
    /// Exchanges that reached retirement.
    pub exchanges_retired: u64,
    /// Retirements whose observable result is an error.
    pub exchanges_failed: u64,
    /// Cancellation requests issued for transport operations.
    pub cancellation_requests: u64,
    /// Deadlines applied by the connection machine.
    pub deadline_expirations: u64,
    /// At least one counter overflowed. Counters saturate without changing protocol behavior.
    pub saturated: bool,
}

pub(crate) enum Metric {
    ReadCompletions,
    ReadBytes,
    WriteCompletions,
    WrittenBytes,
    UncertainWrites,
    BodyDeliveries,
    BodyDelivered,
    BodyConsumed,
    ProducerAccepted,
    ExchangesStarted,
    ExchangesRetired,
    ExchangesFailed,
    Cancellations,
    Expirations,
}

#[cfg(feature = "metrics")]
impl Counters {
    #[inline]
    pub(crate) fn add(&mut self, metric: Metric, amount: u64) {
        let counter = match metric {
            Metric::ReadCompletions => &mut self.read_completions,
            Metric::ReadBytes => &mut self.read_bytes,
            Metric::WriteCompletions => &mut self.write_completions,
            Metric::WrittenBytes => &mut self.written_bytes_lower_bound,
            Metric::UncertainWrites => &mut self.uncertain_write_completions,
            Metric::BodyDeliveries => &mut self.body_deliveries,
            Metric::BodyDelivered => &mut self.body_bytes_delivered,
            Metric::BodyConsumed => &mut self.body_bytes_consumed,
            Metric::ProducerAccepted => &mut self.producer_bytes_accepted,
            Metric::ExchangesStarted => &mut self.exchanges_started,
            Metric::ExchangesRetired => &mut self.exchanges_retired,
            Metric::ExchangesFailed => &mut self.exchanges_failed,
            Metric::Cancellations => &mut self.cancellation_requests,
            Metric::Expirations => &mut self.deadline_expirations,
        };
        match counter.checked_add(amount) {
            Some(value) => *counter = value,
            None => {
                *counter = u64::MAX;
                self.saturated = true;
            }
        }
    }
}

/// A coherent, allocation-free copy of the connection's current observations.
///
/// `observed_at` is the last caller-supplied time, not a clock read.
/// Buffer gauges describe visible input and metadata scratch storage, not RSS.
#[cfg(feature = "metrics")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct MetricsSnapshot {
    /// Identity of the observed connection.
    pub connection: ConnectionId,
    /// `true` for a server-side connection, `false` for a client.
    pub server: bool,
    /// Last caller-supplied time observed by the core.
    pub observed_at: Tick,
    /// Coarse lifecycle phase; not authority to issue a command.
    pub phase: SnapshotPhase,
    /// Current exchange, if one is active.
    pub exchange: Option<ExchangeId>,
    /// Unconsumed input, including bytes in an outstanding body lease.
    pub buffered_input_bytes: usize,
    /// Receive credit currently granted to the body consumer.
    pub body_credit: usize,
    /// Bytes currently occupied in HTTP metadata scratch storage.
    pub metadata_buffer_bytes: usize,
    /// Allocated capacity of HTTP metadata scratch storage.
    pub metadata_buffer_capacity: usize,
    /// The read lane owns an external read or readiness operation.
    pub read_outstanding: bool,
    /// The write lane owns an external write or readiness operation.
    pub write_outstanding: bool,
    /// Whether received body storage is currently leased to a consumer.
    pub body_lease_outstanding: bool,
    /// Primary connection failure, if one has been recorded.
    pub failure: Option<Failure>,
    /// Cumulative connection counters.
    pub counters: Counters,
}

/// Content-free observations emitted during `next`, immediately before the
/// corresponding capability callback. No event queue or suspension is involved.
///
/// A primary failure is reported once, before the first subsequent drive
/// transition. API attempts and superseded deadline candidates are not a trace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LogEvent {
    /// First terminal error recorded for the connection.
    PrimaryFailure(Failure),
    /// Current effective deadline changed; `None` means disarmed.
    DeadlineChanged(Option<Deadline>),
    /// A transport, readiness, close, or body operation was issued.
    OperationIssued(OperationId),
    /// The core requested cancellation of an outstanding operation.
    CancellationRequested(OperationId),
    /// A request head was parsed and admitted.
    RequestReceived {
        /// Admitted exchange.
        exchange: ExchangeId,
        /// Version from the request line.
        version: Version,
    },
    /// A response head was parsed.
    ResponseReceived {
        /// Exchange associated with the response.
        exchange: ExchangeId,
        /// Status code from the status line.
        status: u16,
        /// Whether this is a non-final informational response.
        informational: bool,
    },
    /// Trailers were parsed at the end of a chunked body.
    TrailersReceived {
        /// Exchange that received the trailers.
        exchange: ExchangeId,
        /// Number of trailer fields.
        fields: usize,
    },
    /// A body lease was offered to the application.
    BodyOffered {
        /// Exchange carrying the body.
        exchange: ExchangeId,
        /// Lease operation identity.
        operation: OperationId,
        /// Bytes in the current offer.
        bytes: usize,
    },
    /// An outgoing body frame received its transport receipt.
    BodyReturned {
        /// Exchange that sent the body.
        exchange: ExchangeId,
        /// Producer submission identity.
        body: BodyId,
        /// Positively confirmed payload bytes, excluding framing.
        accepted: usize,
        /// Whether `accepted` is exact or only a lower bound.
        acceptance: Acceptance,
        /// Final result for this body submission.
        result: Result<(), Failure>,
    },
    /// The complete incoming message body and trailers have been consumed.
    IncomingFinished(ExchangeId),
    /// No more payload is required from the outgoing producer.
    SourceFinished(ExchangeId),
    /// Producer demand is available for the exchange.
    SendReady {
        /// Exchange requesting producer data.
        exchange: ExchangeId,
        /// Maximum payload bytes currently accepted.
        capacity: usize,
    },
    /// An exchange reached its terminal result.
    ExchangeFinished(ExchangeFinished),
    /// A successful protocol upgrade can now be taken.
    UpgradeReady(ExchangeId),
    /// The connection reached terminal close.
    Closed(ConnectionResult),
}

#[cfg(all(test, feature = "metrics"))]
mod tests {
    use super::*;

    #[test]
    fn counters_saturate_without_wrapping_other_fields() {
        let mut counters = Counters {
            read_bytes: u64::MAX - 1,
            ..Counters::default()
        };
        counters.add(Metric::ReadBytes, 1);
        assert!(!counters.saturated);
        counters.add(Metric::ReadBytes, 1);
        counters.add(Metric::WriteCompletions, 1);
        assert_eq!(counters.read_bytes, u64::MAX);
        assert!(counters.saturated);
        assert_eq!(counters.write_completions, 1);
    }
}
