use crate::*;

/// Coarse lifecycle information, not permission to issue a command.
#[cfg(feature = "metrics")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SnapshotPhase {
    Http,
    Draining,
    Upgrading,
    ErrorResponse,
    Closing,
    Closed,
    HandedOff,
}

/// Connection-lifetime counters. Invalid or rejected completions do not count.
#[cfg(feature = "metrics")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct Counters {
    pub read_completions: u64,
    pub read_bytes: u64,
    pub write_completions: u64,
    /// Newly confirmed bytes, including framing. Unknown progress is excluded.
    pub written_bytes_lower_bound: u64,
    pub uncertain_write_completions: u64,
    pub body_deliveries: u64,
    /// Bytes offered through leases, including repeated offers after partial consumption.
    pub body_bytes_delivered: u64,
    pub body_bytes_consumed: u64,
    pub producer_bytes_accepted: u64,
    /// Created exchange records, including records that subsequently fail head admission.
    pub exchanges_started: u64,
    pub exchanges_retired: u64,
    /// Retirements whose observable result is an error.
    pub exchanges_failed: u64,
    pub cancellation_requests: u64,
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
    pub connection: ConnectionId,
    pub server: bool,
    pub observed_at: Tick,
    pub phase: SnapshotPhase,
    pub exchange: Option<ExchangeId>,
    /// Unconsumed input, including bytes in an outstanding body lease.
    pub buffered_input_bytes: usize,
    pub body_credit: usize,
    pub metadata_buffer_bytes: usize,
    pub metadata_buffer_capacity: usize,
    /// The read lane owns an external read or readiness operation.
    pub read_outstanding: bool,
    /// The write lane owns an external write or readiness operation.
    pub write_outstanding: bool,
    pub body_lease_outstanding: bool,
    pub failure: Option<Failure>,
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
    PrimaryFailure(Failure),
    DeadlineChanged(Option<Deadline>),
    OperationIssued(OperationId),
    CancellationRequested(OperationId),
    RequestReceived {
        exchange: ExchangeId,
        version: Version,
    },
    ResponseReceived {
        exchange: ExchangeId,
        status: u16,
        informational: bool,
    },
    TrailersReceived {
        exchange: ExchangeId,
        fields: usize,
    },
    BodyOffered {
        exchange: ExchangeId,
        operation: OperationId,
        bytes: usize,
    },
    BodyReturned {
        exchange: ExchangeId,
        body: BodyId,
        accepted: usize,
        acceptance: Acceptance,
        result: Result<(), Failure>,
    },
    IncomingFinished(ExchangeId),
    SourceFinished(ExchangeId),
    SendReady {
        exchange: ExchangeId,
        capacity: usize,
    },
    ExchangeFinished(ExchangeFinished),
    UpgradeReady(ExchangeId),
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
