//! HTTP/2 flow-control windows, fair scheduling, and timer obligations.

use std::collections::hash_map::Entry;

use rustc_hash::{FxBuildHasher, FxHashMap};
use std::time::{Duration, Instant};

use crate::server::ServerError;
use crate::server::h2::wire::{
    H2_DEFAULT_MAX_ACTIVE_STREAMS, H2_MAX_MAX_FRAME_SIZE, H2_MAX_WINDOW_SIZE, H2Frame, H2FrameType,
    H2Limits,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2FlowControlWindow {
    pub(crate) available: i32,
}

impl H2FlowControlWindow {
    pub fn new(size: u32) -> Result<Self, ServerError> {
        if size > H2_MAX_WINDOW_SIZE {
            return Err(ServerError::FlowControlViolation);
        }
        Ok(Self {
            available: size as i32,
        })
    }

    pub const fn available(self) -> i32 {
        self.available
    }

    pub fn consume(&mut self, amount: usize) -> Result<(), ServerError> {
        let amount = i32::try_from(amount).map_err(|_| ServerError::FlowControlViolation)?;
        if amount > self.available {
            return Err(ServerError::FlowControlViolation);
        }
        self.available -= amount;
        Ok(())
    }

    pub fn increase(&mut self, amount: u32) -> Result<(), ServerError> {
        if amount == 0 {
            return Err(ServerError::FlowControlViolation);
        }
        let amount = i32::try_from(amount).map_err(|_| ServerError::FlowControlViolation)?;
        let next = self
            .available
            .checked_add(amount)
            .ok_or(ServerError::FlowControlViolation)?;
        if next > H2_MAX_WINDOW_SIZE as i32 {
            return Err(ServerError::FlowControlViolation);
        }
        self.available = next;
        Ok(())
    }

    pub fn adjust(&mut self, delta: i32) -> Result<(), ServerError> {
        let next = self
            .available
            .checked_add(delta)
            .ok_or(ServerError::FlowControlViolation)?;
        if next > H2_MAX_WINDOW_SIZE as i32 {
            return Err(ServerError::FlowControlViolation);
        }
        self.available = next;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2ReceiveWindow {
    pub(crate) limit: usize,
    pub(crate) available: usize,
    pub(crate) pending_update: usize,
    pub(crate) extra_credit: usize,
    pub(crate) threshold_divisor: usize,
    pub(crate) adaptive: Option<H2AdaptiveReceiveWindow>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct H2AdaptiveReceiveWindow {
    pub(crate) max_limit: usize,
    pub(crate) target_rtt: Duration,
    pub(crate) sample_started_at: Instant,
    pub(crate) sample_received: usize,
    pub(crate) sample_consumed: usize,
    pub(crate) sample_needs_reanchor: bool,
    pub(crate) fast_turnovers: u8,
    pub(crate) blocked_since: Option<Instant>,
    pub(crate) total_received: u64,
    pub(crate) total_consumed: u64,
    pub(crate) total_blocked: Duration,
    pub(crate) last_blocked: Duration,
    pub(crate) rtt_proxy: Duration,
    pub(crate) estimated_bdp: usize,
    pub(crate) growth_events: u64,
    pub(crate) growth_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct H2ReceiveWindowDiagnostics {
    pub current_window_bytes: usize,
    pub max_window_bytes: usize,
    pub received_bytes: u64,
    pub consumed_bytes: u64,
    pub blocked_time: Duration,
    pub last_blocked_time: Duration,
    pub rtt_proxy: Duration,
    pub estimated_bdp_bytes: usize,
    pub growth_events: u64,
    pub growth_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2SendCapacity {
    pub stream_id: u32,
    pub sendable_bytes: usize,
    pub has_pending_data: bool,
    pub stream_window_blocked: bool,
    pub connection_window_blocked: bool,
}

impl H2SendCapacity {
    pub fn new(
        stream_id: u32,
        pending_bytes: usize,
        stream_window: i32,
        connection_window: usize,
    ) -> Self {
        let stream_credit = usize::try_from(stream_window).unwrap_or(0);
        let sendable_bytes = pending_bytes.min(stream_credit).min(connection_window);
        Self {
            stream_id,
            sendable_bytes,
            has_pending_data: pending_bytes > 0,
            stream_window_blocked: pending_bytes > 0 && stream_window <= 0,
            connection_window_blocked: pending_bytes > 0 && connection_window == 0,
        }
    }
}

/// A state-checked DATA frame header and borrowed-payload length.
///
/// The payload remains owned by the adapter. After the header and the indicated
/// payload prefix have been handed to the transport, the adapter must call the
/// matching connection's `commit_data_frame` method.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2DataFramePlan {
    pub(crate) stream_id: u32,
    pub(crate) payload_len: usize,
    pub(crate) end_stream: bool,
    pub(crate) header: [u8; 9],
}

impl H2DataFramePlan {
    pub const fn stream_id(self) -> u32 {
        self.stream_id
    }

    pub const fn payload_len(self) -> usize {
        self.payload_len
    }

    pub const fn end_stream(self) -> bool {
        self.end_stream
    }

    pub const fn header(&self) -> &[u8; 9] {
        &self.header
    }
}

pub(crate) fn h2_data_frame_plan(
    stream_id: u32,
    payload_len: usize,
    end_stream: bool,
) -> H2DataFramePlan {
    debug_assert!(stream_id != 0 && stream_id <= 0x7fff_ffff);
    debug_assert!(payload_len <= H2_MAX_MAX_FRAME_SIZE);
    let payload_len_u32 = payload_len as u32;
    H2DataFramePlan {
        stream_id,
        payload_len,
        end_stream,
        header: [
            (payload_len_u32 >> 16) as u8,
            (payload_len_u32 >> 8) as u8,
            payload_len_u32 as u8,
            0,
            u8::from(end_stream),
            (stream_id >> 24) as u8 & 0x7f,
            (stream_id >> 16) as u8,
            (stream_id >> 8) as u8,
            stream_id as u8,
        ],
    }
}

/// Inclusive upper bounds for the scheduler's per-stream selection histogram.
pub const H2_SCHEDULER_SELECTION_BUCKET_UPPER_BOUNDS: [u64; 8] = [0, 1, 3, 7, 15, 31, 63, u64::MAX];

/// Mutually exclusive primary reason for a failed HTTP/2 send-capacity pass.
///
/// This exhaustive enum is the closed HTTP/2 window-exhaustion domain:
/// capacity can be blocked by either the stream window or the connection
/// window. Non-window flush stops belong to the adapter's stop-reason type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H2FlowStall {
    StreamWindow,
    ConnectionWindow,
}

impl H2FlowStall {
    /// Classifies one failed pass, giving connection exhaustion precedence.
    pub const fn classify(
        stream_window_blocked: bool,
        connection_window_blocked: bool,
    ) -> Option<Self> {
        if connection_window_blocked {
            Some(Self::ConnectionWindow)
        } else if stream_window_blocked {
            Some(Self::StreamWindow)
        } else {
            None
        }
    }
}

/// Fixed-size, saturating HTTP/2 flow-control counters for one connection.
///
/// Adapters detect protocol events but record all flow events in this native
/// accumulator. The counters cover the connection lifetime and never reset.
/// This is a trusted, low-level counter carrier: callers must classify failed
/// passes connection-first and must record only post-startup WINDOW_UPDATE
/// refunds accepted by the caller-owned downstream output layer. Supported
/// adapters enforce that ownership boundary before invoking the raw mutators.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct H2FlowDiagnostics {
    /// Failed passes attributed to stream-window exhaustion.
    pub stream_window_stalls: u64,
    /// Failed passes attributed to connection-window exhaustion.
    pub connection_window_stalls: u64,
    /// Post-startup stream WINDOW_UPDATE refunds accepted by downstream output.
    pub stream_window_updates: u64,
    /// Post-startup connection WINDOW_UPDATE refunds accepted by downstream output.
    pub connection_window_updates: u64,
    /// Stream receive-credit bytes returned by post-startup WINDOW_UPDATE frames.
    pub stream_window_update_bytes: u64,
    /// Connection receive-credit bytes returned by post-startup WINDOW_UPDATE frames.
    pub connection_window_update_bytes: u64,
}

impl H2FlowDiagnostics {
    /// Records one mutually exclusive failed-pass reason.
    pub fn record_stall(&mut self, reason: H2FlowStall) {
        match reason {
            H2FlowStall::StreamWindow => self.record_stream_window_stall(),
            H2FlowStall::ConnectionWindow => self.record_connection_window_stall(),
        }
    }

    /// Records one failed pass caused by stream-window exhaustion.
    pub fn record_stream_window_stall(&mut self) {
        self.stream_window_stalls = self.stream_window_stalls.saturating_add(1);
    }

    /// Records one failed pass caused by connection-window exhaustion.
    pub fn record_connection_window_stall(&mut self) {
        self.connection_window_stalls = self.connection_window_stalls.saturating_add(1);
    }

    /// Records one classified post-startup receive-credit refund committed to output.
    pub fn record_window_update(&mut self, stream_id: u32, amount: usize) {
        let amount = u64::try_from(amount).unwrap_or(u64::MAX);
        if stream_id == 0 {
            self.connection_window_updates = self.connection_window_updates.saturating_add(1);
            self.connection_window_update_bytes =
                self.connection_window_update_bytes.saturating_add(amount);
        } else {
            self.stream_window_updates = self.stream_window_updates.saturating_add(1);
            self.stream_window_update_bytes =
                self.stream_window_update_bytes.saturating_add(amount);
        }
    }

    /// Materializes a snapshot with current adapter and scheduler gauges.
    pub fn snapshot(
        self,
        pending_capacity_streams: usize,
        scheduler: H2FairStreamSchedulerDiagnostics,
    ) -> H2FlowDiagnosticsSnapshot {
        H2FlowDiagnosticsSnapshot {
            stream_window_stalls: self.stream_window_stalls,
            connection_window_stalls: self.connection_window_stalls,
            stream_window_updates: self.stream_window_updates,
            connection_window_updates: self.connection_window_updates,
            stream_window_update_bytes: self.stream_window_update_bytes,
            connection_window_update_bytes: self.connection_window_update_bytes,
            pending_capacity_streams,
            scheduler,
        }
    }
}

/// Runtime-neutral HTTP/2 flow-control and fair-scheduler snapshot.
///
/// Counter fields are saturating connection-lifetime values. Gauge fields
/// describe the connection when the snapshot was materialized.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct H2FlowDiagnosticsSnapshot {
    /// Failed passes attributed to stream-window exhaustion.
    pub stream_window_stalls: u64,
    /// Failed passes attributed to connection-window exhaustion.
    pub connection_window_stalls: u64,
    /// Post-startup stream WINDOW_UPDATE refunds accepted by downstream output.
    pub stream_window_updates: u64,
    /// Post-startup connection WINDOW_UPDATE refunds accepted by downstream output.
    pub connection_window_updates: u64,
    /// Stream receive-credit bytes returned by post-startup WINDOW_UPDATE frames.
    pub stream_window_update_bytes: u64,
    /// Connection receive-credit bytes returned by post-startup WINDOW_UPDATE frames.
    pub connection_window_update_bytes: u64,
    /// Enrolled streams with pending DATA and no applicable send credit.
    pub pending_capacity_streams: usize,
    /// Lifetime fair-scheduler diagnostics for the connection.
    pub scheduler: H2FairStreamSchedulerDiagnostics,
}

/// Bounded scheduler counters and lifetime scheduler-turn distribution.
///
/// All `u64` counters saturate. The fixed histogram describes successful fair
/// scheduler turns; it is workload-shape telemetry, not by itself proof of
/// fairness because it does not encode stream demand or eligibility time.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct H2FairStreamSchedulerDiagnostics {
    /// Number of stream IDs observed over the connection lifetime.
    ///
    /// Includes both currently tracked and retired streams and saturates at
    /// `usize::MAX`.
    pub observed_streams: usize,
    /// Streams currently retained, including queued and temporarily drained streams.
    pub tracked_streams: usize,
    /// Streams currently enrolled for arbitration, including capacity-blocked streams.
    pub queued_streams: usize,
    /// Connection-lifetime calls to [`H2FairStreamScheduler::next_ready`].
    pub selection_attempts: u64,
    /// Connection-lifetime successful fair selections returned by `next_ready`.
    pub selections: u64,
    /// Direct flush invocations that emitted at least one DATA frame.
    ///
    /// This is neither a DATA-frame count nor a fair-turn-equivalent
    /// denominator. It does not contribute to `selections` or the histogram.
    pub immediate_dispatches: u64,
    /// Connection-lifetime fair attempts that found no eligible stream.
    pub no_ready_attempts: u64,
    /// Connection-lifetime live-stream readiness probes that returned false.
    ///
    /// The intrusive queue contains no stale entries, so this excludes no
    /// hidden cleanup work.
    pub skipped_streams: u64,
    /// Maximum live-stream readiness probes performed by one fair attempt.
    pub max_scan_depth: usize,
    /// Minimum fair selections for an observed stream, or zero if none exist.
    pub min_stream_selections: u64,
    /// Maximum fair selections for an observed stream, or zero if none exist.
    pub max_stream_selections: u64,
    /// Observed active and retired streams grouped by inclusive fair-turn bounds.
    ///
    /// Bucket counts saturate at `usize::MAX`.
    pub stream_selection_buckets: [usize; H2_SCHEDULER_SELECTION_BUCKET_UPPER_BOUNDS.len()],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct H2ScheduledStream {
    pub(crate) stream_id: u32,
    pub(crate) queued: bool,
    pub(crate) previous: Option<usize>,
    pub(crate) next: Option<usize>,
    pub(crate) selections: u64,
}

pub(crate) type H2StreamSlots = FxHashMap<u32, usize>;

/// Connection-local round-robin scheduler for HTTP/2 stream send opportunities.
///
/// `register` enrolls a stream, `mark_drained` temporarily removes it from
/// arbitration while retaining its lifetime selection count, and `remove`
/// retires that count permanently. All lifecycle operations are idempotent.
///
/// One scheduler belongs to exactly one HTTP/2 connection. Within that
/// connection, stream IDs are monotonic and an ID must never be registered
/// again after `remove`. A drained but not removed stream may be re-registered.
/// Network-facing owners enforce their configured active-stream limit before
/// calling `register`.
/// The intrusive queue has one physical entry per queued stream, so lifecycle
/// churn cannot retain stale queue tokens. Equality compares operational queue
/// state and intentionally ignores diagnostic history.
#[derive(Clone, Debug, Default)]
pub struct H2FairStreamScheduler {
    pub(crate) stream_slots: H2StreamSlots,
    pub(crate) slots: Vec<H2ScheduledStream>,
    pub(crate) free_head: Option<usize>,
    pub(crate) ready_head: Option<usize>,
    pub(crate) ready_tail: Option<usize>,
    pub(crate) queued_streams: usize,
    pub(crate) selection_attempts: u64,
    pub(crate) selections: u64,
    pub(crate) immediate_dispatches: u64,
    pub(crate) no_ready_attempts: u64,
    pub(crate) skipped_streams: u64,
    pub(crate) max_scan_depth: usize,
    pub(crate) retired_streams: usize,
    pub(crate) retired_min_stream_selections: u64,
    pub(crate) retired_max_stream_selections: u64,
    pub(crate) retired_stream_selection_buckets:
        [usize; H2_SCHEDULER_SELECTION_BUCKET_UPPER_BOUNDS.len()],
}

impl PartialEq for H2FairStreamScheduler {
    fn eq(&self, other: &Self) -> bool {
        self.slot_stream_id(self.ready_head) == other.slot_stream_id(other.ready_head)
            && self.slot_stream_id(self.ready_tail) == other.slot_stream_id(other.ready_tail)
            && self.queued_streams == other.queued_streams
            && self.stream_slots.len() == other.stream_slots.len()
            && self.stream_slots.iter().all(|(stream_id, slot)| {
                let stream = &self.slots[*slot];
                other.stream_slots.get(stream_id).is_some_and(|other_slot| {
                    let other_stream = &other.slots[*other_slot];
                    stream.queued == other_stream.queued
                        && self.slot_stream_id(stream.previous)
                            == other.slot_stream_id(other_stream.previous)
                        && self.slot_stream_id(stream.next)
                            == other.slot_stream_id(other_stream.next)
                })
            })
    }
}

impl Eq for H2FairStreamScheduler {}

impl H2FairStreamScheduler {
    /// Creates a scheduler pre-sized up to the default active-stream cap.
    ///
    /// The map reserves additional deletion headroom so repeated full-cap
    /// generations can reuse storage without rehash allocation. Larger logical
    /// caps grow on demand instead of creating unbounded eager reservations.
    pub fn with_capacity(max_active_streams: usize) -> Self {
        let initial_capacity = max_active_streams.min(H2_DEFAULT_MAX_ACTIVE_STREAMS);
        Self {
            stream_slots: FxHashMap::with_capacity_and_hasher(
                initial_capacity.saturating_mul(2),
                FxBuildHasher,
            ),
            slots: Vec::with_capacity(initial_capacity),
            ..Self::default()
        }
    }

    /// Enrolls a new or temporarily drained stream at the back of the queue.
    ///
    /// A stream ID retired by [`Self::remove`] must not be registered again.
    pub fn register(&mut self, stream_id: u32) {
        let slot = match self.stream_slots.entry(stream_id) {
            Entry::Occupied(entry) => {
                let slot = *entry.get();
                if self.slots[slot].queued {
                    return;
                }
                slot
            }
            Entry::Vacant(entry) => {
                let stream = H2ScheduledStream {
                    stream_id,
                    queued: false,
                    previous: None,
                    next: None,
                    selections: 0,
                };
                let slot = if let Some(slot) = self.free_head {
                    self.free_head = self.slots[slot].next;
                    self.slots[slot] = stream;
                    slot
                } else {
                    let slot = self.slots.len();
                    self.slots.push(stream);
                    slot
                };
                entry.insert(slot);
                slot
            }
        };
        self.enqueue(slot);
    }

    /// Retires a stream and contributes its fair-selection count exactly once.
    pub fn remove(&mut self, stream_id: u32) {
        let Some(slot) = self.stream_slots.remove(&stream_id) else {
            return;
        };
        self.unlink(slot);
        let selections = self.slots[slot].selections;
        if self.retired_streams == 0 {
            self.retired_min_stream_selections = selections;
        } else {
            self.retired_min_stream_selections = self.retired_min_stream_selections.min(selections);
        }
        self.retired_max_stream_selections = self.retired_max_stream_selections.max(selections);
        self.retired_streams = self.retired_streams.saturating_add(1);
        let bucket = h2_scheduler_selection_bucket(selections);
        self.retired_stream_selection_buckets[bucket] =
            self.retired_stream_selection_buckets[bucket].saturating_add(1);

        let stream = &mut self.slots[slot];
        stream.previous = None;
        stream.next = self.free_head;
        stream.selections = 0;
        self.free_head = Some(slot);
    }

    /// Selects the next eligible stream in round-robin order.
    pub fn next_ready<F>(&mut self, mut is_ready: F) -> Option<u32>
    where
        F: FnMut(u32) -> bool,
    {
        self.selection_attempts = self.selection_attempts.saturating_add(1);
        let len = self.queued_streams;
        let mut scan_depth = 0usize;
        for _ in 0..len {
            let slot = self.ready_head?;
            let stream_id = self.slots[slot].stream_id;
            self.rotate_front_to_back(slot);
            scan_depth = scan_depth.saturating_add(1);
            if is_ready(stream_id) {
                let stream = &mut self.slots[slot];
                stream.selections = stream.selections.saturating_add(1);
                self.selections = self.selections.saturating_add(1);
                self.max_scan_depth = self.max_scan_depth.max(scan_depth);
                return Some(stream_id);
            }
            self.skipped_streams = self.skipped_streams.saturating_add(1);
        }
        self.max_scan_depth = self.max_scan_depth.max(scan_depth);
        self.no_ready_attempts = self.no_ready_attempts.saturating_add(1);
        None
    }

    /// Records an immediate adapter dispatch that emitted DATA.
    pub fn record_immediate_dispatch(&mut self) {
        self.immediate_dispatches = self.immediate_dispatches.saturating_add(1);
    }

    /// Temporarily removes a drained stream from fair arbitration in O(1).
    pub fn mark_drained(&mut self, stream_id: u32) {
        if let Some(slot) = self.stream_slots.get(&stream_id).copied() {
            self.unlink(slot);
        }
    }

    /// Returns the number of streams enrolled for fair arbitration.
    pub fn len(&self) -> usize {
        self.queued_streams
    }

    /// Returns whether no stream is enrolled for fair arbitration.
    pub fn is_empty(&self) -> bool {
        self.queued_streams == 0
    }

    /// Counts enrolled streams matching an adapter-defined capacity predicate.
    pub fn pending_capacity_streams<F>(&self, mut is_pending: F) -> usize
    where
        F: FnMut(u32) -> bool,
    {
        self.stream_slots
            .iter()
            .filter(|(stream_id, slot)| self.slots[**slot].queued && is_pending(**stream_id))
            .count()
    }

    /// Returns fixed-size lifetime scheduling diagnostics without allocation.
    pub fn diagnostics(&self) -> H2FairStreamSchedulerDiagnostics {
        let mut diagnostics = H2FairStreamSchedulerDiagnostics {
            observed_streams: self.retired_streams.saturating_add(self.stream_slots.len()),
            tracked_streams: self.stream_slots.len(),
            queued_streams: self.queued_streams,
            selection_attempts: self.selection_attempts,
            selections: self.selections,
            immediate_dispatches: self.immediate_dispatches,
            no_ready_attempts: self.no_ready_attempts,
            skipped_streams: self.skipped_streams,
            max_scan_depth: self.max_scan_depth,
            min_stream_selections: self.retired_min_stream_selections,
            max_stream_selections: self.retired_max_stream_selections,
            stream_selection_buckets: self.retired_stream_selection_buckets,
        };

        if diagnostics.observed_streams == 0 {
            return diagnostics;
        }

        if self.retired_streams == 0 {
            diagnostics.min_stream_selections = u64::MAX;
        }
        for slot in self.stream_slots.values() {
            let stream = &self.slots[*slot];
            diagnostics.min_stream_selections =
                diagnostics.min_stream_selections.min(stream.selections);
            diagnostics.max_stream_selections =
                diagnostics.max_stream_selections.max(stream.selections);
            let bucket = h2_scheduler_selection_bucket(stream.selections);
            diagnostics.stream_selection_buckets[bucket] =
                diagnostics.stream_selection_buckets[bucket].saturating_add(1);
        }
        diagnostics
    }

    pub(crate) fn enqueue(&mut self, slot: usize) {
        let previous = self.ready_tail;
        if let Some(tail) = previous {
            self.slots[tail].next = Some(slot);
        } else {
            self.ready_head = Some(slot);
        }
        let stream = &mut self.slots[slot];
        stream.queued = true;
        stream.previous = previous;
        stream.next = None;
        self.ready_tail = Some(slot);
        self.queued_streams = self.queued_streams.saturating_add(1);
    }

    pub(crate) fn unlink(&mut self, slot: usize) {
        let stream = &self.slots[slot];
        if !stream.queued {
            return;
        }
        let previous = stream.previous;
        let next = stream.next;

        if let Some(previous) = previous {
            self.slots[previous].next = next;
        } else {
            self.ready_head = next;
        }
        if let Some(next) = next {
            self.slots[next].previous = previous;
        } else {
            self.ready_tail = previous;
        }
        let stream = &mut self.slots[slot];
        stream.queued = false;
        stream.previous = None;
        stream.next = None;
        self.queued_streams = self.queued_streams.saturating_sub(1);
    }

    pub(crate) fn rotate_front_to_back(&mut self, slot: usize) {
        if self.ready_head == self.ready_tail {
            return;
        }
        let next = self.slots[slot]
            .next
            .expect("multi-stream scheduler head has a successor");
        let tail = self.ready_tail.expect("non-empty scheduler has a tail");

        self.slots[next].previous = None;
        self.slots[tail].next = Some(slot);
        let stream = &mut self.slots[slot];
        stream.previous = Some(tail);
        stream.next = None;
        self.ready_head = Some(next);
        self.ready_tail = Some(slot);
    }

    pub(crate) fn slot_stream_id(&self, slot: Option<usize>) -> Option<u32> {
        slot.map(|slot| self.slots[slot].stream_id)
    }
}

pub(crate) fn h2_scheduler_selection_bucket(selections: u64) -> usize {
    H2_SCHEDULER_SELECTION_BUCKET_UPPER_BOUNDS
        .iter()
        .position(|upper_bound| selections <= *upper_bound)
        .expect("final scheduler selection bucket is unbounded")
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct H2BackpressureState {
    pub pending_streams: usize,
    pub queued_data_bytes: usize,
    pub queued_control_frames: usize,
    pub data_over_limit: bool,
    pub control_over_limit: bool,
}

impl H2BackpressureState {
    pub fn new(
        pending_streams: usize,
        queued_data_bytes: usize,
        queued_control_frames: usize,
        limits: H2Limits,
    ) -> Self {
        Self {
            pending_streams,
            queued_data_bytes,
            queued_control_frames,
            data_over_limit: queued_data_bytes > limits.max_queued_data_bytes,
            control_over_limit: queued_control_frames > limits.max_queued_control_frames,
        }
    }

    pub const fn should_read_more(self) -> bool {
        !self.data_over_limit && !self.control_over_limit
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum H2SettingsSyncState {
    #[default]
    Synced,
    WaitingAck,
}

impl H2SettingsSyncState {
    pub(crate) const fn from_pending_acks(pending: u8) -> Self {
        if pending == 0 {
            Self::Synced
        } else {
            Self::WaitingAck
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct H2SettingsAckDebt {
    pub(crate) pending: u8,
    pub(crate) timeout_emitted: bool,
}

impl H2SettingsAckDebt {
    pub(crate) const fn sync_state(self) -> H2SettingsSyncState {
        H2SettingsSyncState::from_pending_acks(self.pending)
    }

    pub(crate) const fn timer_owed(self) -> bool {
        self.pending > 0 && !self.timeout_emitted
    }

    pub(crate) fn note_sent(&mut self) {
        self.pending = self.pending.saturating_add(1);
        self.timeout_emitted = false;
    }

    pub(crate) fn note_ack(&mut self) -> Result<(), ServerError> {
        self.pending = self
            .pending
            .checked_sub(1)
            .ok_or(ServerError::InvalidFrame)?;
        if self.pending == 0 {
            self.timeout_emitted = false;
        }
        Ok(())
    }

    pub(crate) fn note_timeout(&mut self) {
        self.timeout_emitted = true;
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct H2ControlDiagnostics {
    pub settings_frames: u64,
    pub settings_acks: u64,
    pub pings: u64,
    pub ping_acks: u64,
    pub resets: u64,
    pub goaways: u64,
    pub ignored_frames: u64,
    pub control_rejections: u64,
    pub settings_rejections: u64,
    pub window_update_rejections: u64,
    pub ping_rejections: u64,
    pub reset_rejections: u64,
    pub goaway_rejections: u64,
    pub priority_rejections: u64,
}

impl H2ControlDiagnostics {
    pub(crate) fn record_rejection(&mut self, frame_type: H2FrameType) {
        self.control_rejections = self.control_rejections.saturating_add(1);
        match frame_type {
            H2FrameType::Settings => {
                self.settings_rejections = self.settings_rejections.saturating_add(1);
            }
            H2FrameType::WindowUpdate => {
                self.window_update_rejections = self.window_update_rejections.saturating_add(1);
            }
            H2FrameType::Ping => {
                self.ping_rejections = self.ping_rejections.saturating_add(1);
            }
            H2FrameType::RstStream => {
                self.reset_rejections = self.reset_rejections.saturating_add(1);
            }
            H2FrameType::Goaway => {
                self.goaway_rejections = self.goaway_rejections.saturating_add(1);
            }
            H2FrameType::Priority => {
                self.priority_rejections = self.priority_rejections.saturating_add(1);
            }
            H2FrameType::Data
            | H2FrameType::Headers
            | H2FrameType::Continuation
            | H2FrameType::PushPromise
            | H2FrameType::Unknown(_) => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H2TimerIntent {
    SettingsAckTimeout,
    GracefulShutdownPing,
}

/// Independent HTTP/2 timer obligations owed by a connection.
///
/// Adapters arm each set flag separately. [`H2TimerIntent`] remains the
/// staged primary projection when a caller can track only one timer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct H2TimerObligations {
    pub(crate) settings_ack: bool,
    pub(crate) graceful_shutdown_ping: bool,
}

impl H2TimerObligations {
    /// No outstanding protocol timers.
    pub const fn empty() -> Self {
        Self {
            settings_ack: false,
            graceful_shutdown_ping: false,
        }
    }

    /// A local SETTINGS frame is waiting for an acknowledgement.
    pub const fn settings_ack(self) -> bool {
        self.settings_ack
    }

    /// Graceful shutdown is waiting for the round-trip PING acknowledgement.
    pub const fn graceful_shutdown_ping(self) -> bool {
        self.graceful_shutdown_ping
    }

    /// True when no protocol timer is owed.
    pub const fn is_empty(self) -> bool {
        !self.settings_ack && !self.graceful_shutdown_ping
    }

    pub(crate) const fn primary_intent(self) -> Option<H2TimerIntent> {
        if self.graceful_shutdown_ping {
            Some(H2TimerIntent::GracefulShutdownPing)
        } else if self.settings_ack {
            Some(H2TimerIntent::SettingsAckTimeout)
        } else {
            None
        }
    }
}

pub(crate) const H2_GRACEFUL_SHUTDOWN_LAST_STREAM_ID: u32 = 0x7fff_ffff;
pub(crate) const H2_GRACEFUL_SHUTDOWN_PING_PAYLOAD: [u8; 8] = *b"kimojio!";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum H2OutboundShutdown {
    #[default]
    Open,
    GracefulPingPending,
    GoawaySent {
        last_stream_id: u32,
    },
}

impl H2OutboundShutdown {
    pub(crate) const fn sent_last_stream_id(self) -> Option<u32> {
        match self {
            Self::Open => None,
            Self::GracefulPingPending => Some(H2_GRACEFUL_SHUTDOWN_LAST_STREAM_ID),
            Self::GoawaySent { last_stream_id } => Some(last_stream_id),
        }
    }
}

pub(crate) fn encode_h2_goaway(output: &mut Vec<u8>, last_stream_id: u32, error_code: u32) {
    H2Frame {
        frame_type: H2FrameType::Goaway,
        flags: 0,
        stream_id: 0,
        payload: [
            (last_stream_id & 0x7fff_ffff).to_be_bytes(),
            error_code.to_be_bytes(),
        ]
        .concat(),
    }
    .encode(output);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H2ShutdownIntent {
    None,
    Drain { last_stream_id: u32 },
    Close,
}

impl H2ReceiveWindow {
    pub const fn new(limit: usize) -> Self {
        Self {
            limit,
            available: limit,
            pending_update: 0,
            extra_credit: 0,
            threshold_divisor: 4,
            adaptive: None,
        }
    }

    pub fn with_adaptive_growth(
        limit: usize,
        max_limit: usize,
        target_rtt: Duration,
        now: Instant,
    ) -> Result<Self, ServerError> {
        if limit == 0
            || limit > H2_MAX_WINDOW_SIZE as usize
            || max_limit < limit
            || max_limit > H2_MAX_WINDOW_SIZE as usize
            || target_rtt.is_zero()
        {
            return Err(ServerError::InvalidFrame);
        }
        Ok(Self {
            adaptive: Some(H2AdaptiveReceiveWindow {
                max_limit,
                target_rtt,
                sample_started_at: now,
                sample_received: 0,
                sample_consumed: 0,
                sample_needs_reanchor: false,
                fast_turnovers: 0,
                blocked_since: None,
                total_received: 0,
                total_consumed: 0,
                total_blocked: Duration::ZERO,
                last_blocked: Duration::ZERO,
                rtt_proxy: target_rtt,
                estimated_bdp: limit,
                growth_events: 0,
                growth_bytes: 0,
            }),
            ..Self::new(limit)
        })
    }

    pub const fn limit(self) -> usize {
        self.limit
    }

    pub const fn available(self) -> usize {
        self.available
    }

    pub const fn pending_update(self) -> usize {
        self.pending_update
    }

    pub fn receive_data(&mut self, amount: usize) -> Result<(), ServerError> {
        if self.adaptive.is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        self.receive_data_inner(amount)
    }

    pub fn receive_data_at(&mut self, amount: usize, now: Instant) -> Result<(), ServerError> {
        if amount > self.available {
            return Err(ServerError::InvalidFrame);
        }
        if let Some(mut adaptive) = self.adaptive {
            if amount > 0 && (adaptive.total_received == 0 || adaptive.sample_needs_reanchor) {
                adaptive.sample_started_at = now;
                adaptive.sample_needs_reanchor = false;
            }
            if let Some(blocked_since) = adaptive.blocked_since.take() {
                let blocked = now
                    .checked_duration_since(blocked_since)
                    .unwrap_or_default();
                adaptive.last_blocked = blocked;
                adaptive.total_blocked = adaptive.total_blocked.saturating_add(blocked);
                adaptive.rtt_proxy = adaptive.target_rtt.max(blocked);
            }
            let elapsed = now
                .checked_duration_since(adaptive.sample_started_at)
                .unwrap_or_default();
            if elapsed > adaptive.target_rtt && adaptive.sample_received < self.limit {
                adaptive.sample_started_at = now;
                adaptive.sample_received = 0;
                adaptive.sample_consumed = 0;
                adaptive.fast_turnovers = 0;
            }
            adaptive.sample_received = adaptive.sample_received.saturating_add(amount);
            adaptive.total_received = adaptive
                .total_received
                .saturating_add(u64::try_from(amount).unwrap_or(u64::MAX));
            self.adaptive = Some(adaptive);
        }
        self.receive_data_inner(amount)?;
        if self.available == 0
            && let Some(adaptive) = self.adaptive.as_mut()
            && adaptive.blocked_since.is_none()
        {
            adaptive.blocked_since = Some(now);
        }
        Ok(())
    }

    pub(crate) fn receive_data_inner(&mut self, amount: usize) -> Result<(), ServerError> {
        if amount > self.available {
            return Err(ServerError::InvalidFrame);
        }
        self.available -= amount;
        Ok(())
    }

    pub fn grant_extra_credit_for_frame(
        &mut self,
        frame_len: usize,
        buffered_len: usize,
    ) -> Result<Option<usize>, ServerError> {
        let remaining = frame_len.saturating_sub(buffered_len);
        if remaining <= self.available {
            return Ok(None);
        }
        let needed = remaining - self.available;
        let normal_credit = self.pending_update.min(needed);
        if normal_credit > 0 {
            self.pending_update -= normal_credit;
            self.available = self
                .available
                .checked_add(normal_credit)
                .ok_or(ServerError::InvalidFrame)?;
        }
        let extra_needed = needed - normal_credit;
        if extra_needed > 0 {
            self.grant_extra_credit(extra_needed)?;
        }
        Ok(Some(needed))
    }

    pub fn grant_extra_credit(&mut self, amount: usize) -> Result<Option<usize>, ServerError> {
        if amount == 0 {
            return Ok(None);
        }
        let available = self
            .available
            .checked_add(amount)
            .ok_or(ServerError::InvalidFrame)?;
        if available > H2_MAX_WINDOW_SIZE as usize {
            return Err(ServerError::InvalidFrame);
        }
        self.available = available;
        self.extra_credit = self
            .extra_credit
            .checked_add(amount)
            .ok_or(ServerError::InvalidFrame)?;
        Ok(Some(amount))
    }

    /// Rolls back a DATA debit rejected before the owner accepts its payload.
    ///
    /// Unlike [`Self::grant_extra_credit`], rollback restores existing credit
    /// without creating debt that a later valid consumption must absorb.
    pub fn rollback_received_data(&mut self, amount: usize) -> Result<Option<usize>, ServerError> {
        if amount == 0 {
            return Ok(None);
        }
        let available = self
            .available
            .checked_add(amount)
            .ok_or(ServerError::InvalidFrame)?;
        if available > H2_MAX_WINDOW_SIZE as usize {
            return Err(ServerError::InvalidFrame);
        }
        self.available = available;
        if let Some(adaptive) = self.adaptive.as_mut() {
            adaptive.sample_received = adaptive.sample_received.saturating_sub(amount);
            if adaptive.sample_received == 0 && adaptive.total_consumed == 0 {
                adaptive.sample_needs_reanchor = true;
            }
            if self.available > 0 {
                adaptive.blocked_since = None;
            }
        }
        Ok(Some(amount))
    }

    pub fn consume_data(&mut self, amount: usize) -> Result<Option<usize>, ServerError> {
        if self.adaptive.is_some() {
            return Err(ServerError::InvalidOutboundState);
        }
        self.consume_data_inner(amount)
    }

    pub fn consume_data_at(
        &mut self,
        amount: usize,
        now: Instant,
    ) -> Result<Option<usize>, ServerError> {
        if let Some(adaptive) = self.adaptive.as_mut() {
            adaptive.sample_consumed = adaptive.sample_consumed.saturating_add(amount);
            adaptive.total_consumed = adaptive
                .total_consumed
                .saturating_add(u64::try_from(amount).unwrap_or(u64::MAX));
        }
        let update = self.consume_data_inner(amount)?.unwrap_or(0);
        let growth = self.adaptive_growth(now)?;
        let update = update
            .checked_add(growth)
            .ok_or(ServerError::InvalidFrame)?;
        Ok((update > 0).then_some(update))
    }

    pub(crate) fn consume_data_inner(
        &mut self,
        amount: usize,
    ) -> Result<Option<usize>, ServerError> {
        let mut reclaim = amount;
        if self.extra_credit > 0 {
            let extra = self.extra_credit.min(reclaim);
            self.extra_credit -= extra;
            reclaim -= extra;
        }
        self.pending_update = self
            .pending_update
            .checked_add(reclaim)
            .ok_or(ServerError::InvalidFrame)?;
        let threshold = (self.limit / self.threshold_divisor).max(1);
        if self.pending_update < threshold {
            return Ok(None);
        }
        let update = self.pending_update;
        self.pending_update = 0;
        self.available = self
            .available
            .checked_add(update)
            .ok_or(ServerError::InvalidFrame)?;
        Ok(Some(update))
    }

    pub(crate) fn adaptive_growth(&mut self, now: Instant) -> Result<usize, ServerError> {
        let Some(mut adaptive) = self.adaptive else {
            return Ok(0);
        };
        if adaptive.sample_received < self.limit || adaptive.sample_consumed < self.limit {
            return Ok(0);
        }

        let elapsed = now
            .checked_duration_since(adaptive.sample_started_at)
            .unwrap_or_default();
        let elapsed_nanos = elapsed.as_nanos().max(1);
        let estimated_bdp = (adaptive.sample_received as u128)
            .saturating_mul(adaptive.rtt_proxy.as_nanos())
            .checked_div(elapsed_nanos)
            .unwrap_or(u128::MAX)
            .min(H2_MAX_WINDOW_SIZE as u128);
        adaptive.estimated_bdp =
            usize::try_from(estimated_bdp).unwrap_or(H2_MAX_WINDOW_SIZE as usize);

        if elapsed <= adaptive.target_rtt {
            let turnovers = (adaptive.sample_received.min(adaptive.sample_consumed) / self.limit)
                .max(1)
                .min(u8::MAX as usize) as u8;
            adaptive.fast_turnovers = adaptive.fast_turnovers.saturating_add(turnovers);
        } else {
            adaptive.fast_turnovers = 0;
        }
        adaptive.sample_started_at = now;
        adaptive.sample_received = 0;
        adaptive.sample_consumed = 0;

        let mut growth = 0;
        if adaptive.fast_turnovers >= 2 && self.limit < adaptive.max_limit {
            let minimum_target = self.limit.saturating_add((self.limit / 2).max(1));
            let maximum_target = self.limit.saturating_mul(2);
            let target = adaptive
                .estimated_bdp
                .max(minimum_target)
                .min(maximum_target)
                .min(adaptive.max_limit);
            growth = target.saturating_sub(self.limit);
            if growth > 0 {
                let available = self
                    .available
                    .checked_add(growth)
                    .ok_or(ServerError::InvalidFrame)?;
                if available > H2_MAX_WINDOW_SIZE as usize {
                    return Err(ServerError::InvalidFrame);
                }
                self.available = available;
                self.limit = target;
                adaptive.growth_events = adaptive.growth_events.saturating_add(1);
                adaptive.growth_bytes = adaptive
                    .growth_bytes
                    .saturating_add(u64::try_from(growth).unwrap_or(u64::MAX));
                adaptive.fast_turnovers = 0;
            }
        }
        self.adaptive = Some(adaptive);
        Ok(growth)
    }

    pub fn diagnostics(self) -> H2ReceiveWindowDiagnostics {
        let Some(adaptive) = self.adaptive else {
            return H2ReceiveWindowDiagnostics {
                current_window_bytes: self.limit,
                max_window_bytes: self.limit,
                ..H2ReceiveWindowDiagnostics::default()
            };
        };
        H2ReceiveWindowDiagnostics {
            current_window_bytes: self.limit,
            max_window_bytes: adaptive.max_limit,
            received_bytes: adaptive.total_received,
            consumed_bytes: adaptive.total_consumed,
            blocked_time: adaptive.total_blocked,
            last_blocked_time: adaptive.last_blocked,
            rtt_proxy: adaptive.rtt_proxy,
            estimated_bdp_bytes: adaptive.estimated_bdp,
            growth_events: adaptive.growth_events,
            growth_bytes: adaptive.growth_bytes,
        }
    }
}
