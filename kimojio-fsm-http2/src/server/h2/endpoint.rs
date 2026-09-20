//! Shared HTTP/2 connection endpoint state.

use std::borrow::Cow;
use std::collections::{VecDeque, hash_map::Entry};

use rustc_hash::FxHashMap;
use std::time::Duration;

use crate::Header;
use crate::HttpLimits;
use crate::server::ServerError;
use crate::server::h2::client::H2Client;
use crate::server::h2::compact_headers::CompactHeaderFields;
use crate::server::h2::events::H2InitialWindowSizeChange;
use crate::server::h2::flow::*;
use crate::server::h2::headers::*;
use crate::server::h2::server::H2Server;
use crate::server::h2::wire::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2OutboundCommit {
    pub(crate) sequence: u64,
}

impl H2OutboundCommit {
    /// Monotonic connection-local wire-order sequence.
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

/// Borrowed view of the next complete outbound transaction in connection wire order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2OutboundBlockRef<'a> {
    pub(crate) commit: H2OutboundCommit,
    pub(crate) bytes: &'a [u8],
}

impl<'a> H2OutboundBlockRef<'a> {
    /// Receipt that must be acknowledged after this block is handed to the transport.
    pub const fn commit(self) -> H2OutboundCommit {
        self.commit
    }

    /// Complete serialized transaction bytes.
    pub const fn bytes(self) -> &'a [u8] {
        self.bytes
    }
}

#[derive(Debug)]
pub(crate) struct H2QueuedOutboundBlock {
    pub(crate) commit: H2OutboundCommit,
    pub(crate) bytes: Vec<u8>,
}

#[cfg(feature = "hpack-test-support")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct H2OutboundEncodingObservations {
    pub(crate) temporary_hpack_block_allocations: usize,
    pub(crate) block_to_frame_payload_copy_bytes: usize,
    pub(crate) queue_owned_frame_allocations: usize,
}

#[derive(Debug, Default)]
pub(crate) struct H2OutboundQueue {
    pub(crate) blocks: VecDeque<H2QueuedOutboundBlock>,
    pub(crate) next_sequence: u64,
    #[cfg(feature = "hpack-test-support")]
    pub(crate) allocation_failure_after: Option<usize>,
}

impl H2OutboundQueue {
    pub(crate) fn reserve_commit(&mut self) -> Result<H2OutboundCommit, H2HpackError> {
        let commit = H2OutboundCommit {
            sequence: self
                .next_sequence
                .checked_add(1)
                .ok_or(H2HpackError::StateOverflow)?,
        };
        if self.blocks.len() == self.blocks.capacity() {
            #[cfg(feature = "hpack-test-support")]
            if let Some(remaining) = self.allocation_failure_after.as_mut() {
                if *remaining == 0 {
                    return Err(H2HpackError::AllocationFailed);
                }
                *remaining -= 1;
            }
            self.blocks
                .try_reserve(1)
                .map_err(|_| H2HpackError::AllocationFailed)?;
        }
        Ok(commit)
    }

    pub(crate) fn push_reserved(&mut self, commit: H2OutboundCommit, bytes: Vec<u8>) {
        debug_assert_eq!(commit.sequence, self.next_sequence + 1);
        self.next_sequence = commit.sequence;
        self.blocks
            .push_back(H2QueuedOutboundBlock { commit, bytes });
    }

    pub(crate) fn front(&self) -> Option<H2OutboundBlockRef<'_>> {
        self.blocks.front().map(|block| H2OutboundBlockRef {
            commit: block.commit,
            bytes: &block.bytes,
        })
    }

    pub(crate) fn acknowledge(&mut self, commit: H2OutboundCommit) -> Result<(), ServerError> {
        if self
            .blocks
            .front()
            .is_none_or(|block| block.commit != commit)
        {
            return Err(ServerError::InvalidOutboundState);
        }
        self.blocks.pop_front();
        Ok(())
    }

    pub(crate) fn clear_headers_end_stream(
        &mut self,
        commit: H2OutboundCommit,
    ) -> Result<(), ServerError> {
        let block = self
            .blocks
            .iter_mut()
            .find(|block| block.commit == commit)
            .ok_or(ServerError::InvalidOutboundState)?;
        if block.bytes.len() < 9
            || block.bytes[3] != H2FrameType::Headers.as_u8()
            || block.bytes[4] & 0x1 == 0
        {
            return Err(ServerError::InvalidOutboundState);
        }
        block.bytes[4] &= !0x1;
        Ok(())
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_allocation_failure_after(&mut self, successful_allocations: Option<usize>) {
        self.allocation_failure_after = successful_allocations;
    }
}

pub(crate) enum H2OutboundHeaderBlockTarget<'a> {
    Server(&'a mut H2Server),
    Client(&'a mut H2Client),
}

/// A reversible outbound header transaction.
///
/// Dropping this value abandons the transaction without changing connection
/// compression state. [`Self::commit`] atomically assigns the complete framed
/// block to the owning connection's outbound queue.
pub struct H2OutboundHeaderBlock<'connection, 'headers> {
    pub(crate) target: H2OutboundHeaderBlockTarget<'connection>,
    pub(crate) stream_id: u32,
    pub(crate) headers: &'headers [H2HeaderField],
    pub(crate) end_stream: bool,
}

impl H2OutboundHeaderBlock<'_, '_> {
    /// Assigns this complete block to connection-owned wire order.
    pub fn commit(self) -> Result<H2OutboundCommit, H2ProtocolError> {
        let Self {
            target,
            stream_id,
            headers,
            end_stream,
        } = self;
        match target {
            H2OutboundHeaderBlockTarget::Server(server) => {
                server.enqueue_outbound_header_block(stream_id, headers, end_stream, 0, |_| {})
            }
            H2OutboundHeaderBlockTarget::Client(client) => {
                client.enqueue_outbound_header_block(stream_id, headers, end_stream, 0, |_| {})
            }
        }
    }
}

pub(crate) struct H2ConnectionCodecs {
    pub(crate) inbound: crate::hpack::Decoder,
    pub(crate) outbound: crate::hpack::Encoder,
}

impl H2ConnectionCodecs {
    pub(crate) fn new() -> Self {
        Self {
            inbound: crate::hpack::Decoder::new(),
            outbound: crate::hpack::Encoder::new(),
        }
    }
}

/// Role-neutral connection storage owned by one concrete HTTP/2 endpoint.
///
/// Keep directional events, errors, timers, shutdown, stream creation, and
/// closure policy on [`H2Server`] or [`H2Client`]. Put equivalent connection
/// mechanics here. Use narrow stream selectors only when the stored stream
/// shapes differ.
///
/// The concrete types keep explicit associated forwards for source
/// compatibility. Do not expose this type downstream, implement `Deref`, or
/// replace the forwards with a broad role-policy surface.
///
/// The extraction audit found 40 identical, eight structurally equivalent,
/// eight helper-backed, and 16 role-specific method pairs. New same-named
/// methods must keep role-neutral decisions here and directional decisions on
/// the concrete endpoint.
///
/// SETTINGS preparation leaves outbound HPACK unchanged until window preflight
/// succeeds. DATA commit stays concrete because server and client error-side
/// effects use different mutation orders.
pub(crate) struct H2Endpoint<Stream> {
    pub(crate) now: Duration,
    pub(crate) settings_seen: bool,
    pub(crate) settings: H2Settings,
    pub(crate) local_initial_window_size: u32,
    pub(crate) local_connection_window_size: u32,
    pub(crate) local_enable_push: Option<bool>,
    pub(crate) header_codecs: Option<H2ConnectionCodecs>,
    pub(crate) outbound_queue: Box<H2OutboundQueue>,
    #[cfg(feature = "hpack-test-support")]
    pub(crate) outbound_encoding_observations: H2OutboundEncodingObservations,
    pub(crate) streams: FxHashMap<u32, Stream>,
    pub(crate) send_connection_window: H2FlowControlWindow,
    pub(crate) tombstones: FxHashMap<u32, H2StreamTombstone>,
    pub(crate) closed_stream_order: VecDeque<u32>,
    pub(crate) control_budget: H2ControlFrameBudget,
    pub(crate) limits: H2Limits,
    pub(crate) http_limits: HttpLimits,
    pub(crate) header_block: H2HeaderBlockAssembler,
    pub(crate) last_hpack_error: Option<H2HpackError>,
    pub(crate) decoded_header_scratch: Vec<H2HeaderField>,
    pub(crate) compact_header_scratch: CompactHeaderFields,
    pub(crate) last_protocol_error: Option<H2ProtocolError>,
    pub(crate) last_compat_error: Option<ServerError>,
    pub(crate) terminal_protocol_error: Option<H2ProtocolError>,
    pub(crate) local_settings_ack_debt: H2SettingsAckDebt,
    pub(crate) received_goaway_last_stream_id: Option<u32>,
    pub(crate) outbound_shutdown: H2OutboundShutdown,
    pub(crate) control_diagnostics: H2ControlDiagnostics,
}

impl<Stream> Default for H2Endpoint<Stream> {
    fn default() -> Self {
        Self {
            now: Duration::ZERO,
            settings_seen: false,
            settings: H2Settings::default(),
            local_initial_window_size: H2Settings::default().initial_window_size,
            local_connection_window_size: H2Settings::default().initial_window_size,
            local_enable_push: None,
            header_codecs: Some(H2ConnectionCodecs::new()),
            outbound_queue: Box::default(),
            #[cfg(feature = "hpack-test-support")]
            outbound_encoding_observations: H2OutboundEncodingObservations::default(),
            streams: FxHashMap::default(),
            send_connection_window: H2FlowControlWindow::new(
                H2Settings::default().initial_window_size,
            )
            .expect("the default HTTP/2 window is valid"),
            tombstones: FxHashMap::default(),
            closed_stream_order: VecDeque::new(),
            control_budget: H2ControlFrameBudget::default(),
            limits: H2Limits::default(),
            http_limits: HttpLimits::default(),
            header_block: H2HeaderBlockAssembler::default(),
            last_hpack_error: None,
            decoded_header_scratch: Vec::new(),
            compact_header_scratch: CompactHeaderFields::default(),
            last_protocol_error: None,
            last_compat_error: None,
            terminal_protocol_error: None,
            local_settings_ack_debt: H2SettingsAckDebt::default(),
            received_goaway_last_stream_id: None,
            outbound_shutdown: H2OutboundShutdown::Open,
            control_diagnostics: H2ControlDiagnostics::default(),
        }
    }
}

pub(crate) struct H2PreparedSettings {
    settings: H2Settings,
    previous_initial_window_size: u32,
    table_sizes: Vec<usize>,
}

impl H2PreparedSettings {
    fn initial_window_change(&self) -> Option<H2InitialWindowSizeChange> {
        let current = self.settings.initial_window_size;
        (self.previous_initial_window_size != current).then_some(H2InitialWindowSizeChange {
            previous: self.previous_initial_window_size,
            current,
        })
    }

    fn initial_window_delta(&self) -> Result<Option<i32>, ServerError> {
        let Some(change) = self.initial_window_change() else {
            return Ok(None);
        };
        let delta = i64::from(change.current) - i64::from(change.previous);
        Ok(Some(
            i32::try_from(delta).map_err(|_| ServerError::FlowControlViolation)?,
        ))
    }
}

impl<Stream> H2Endpoint<Stream> {
    pub(crate) fn for_external_hpack_adapter() -> Self {
        Self {
            header_codecs: None,
            ..Self::default()
        }
    }

    pub(crate) fn with_local_flow_control_and_http_limits(
        initial_stream_window: u32,
        initial_connection_window: u32,
        limits: H2Limits,
        http_limits: HttpLimits,
    ) -> Result<Self, ServerError> {
        validate_h2_limits(limits)?;
        let mut settings = H2Settings::default();
        settings.apply(H2Setting::new(
            H2SettingId::InitialWindowSize,
            initial_stream_window,
        ))?;
        if initial_connection_window < H2Settings::default().initial_window_size {
            return Err(ServerError::InvalidFrame);
        }
        let mut endpoint = Self {
            local_initial_window_size: initial_stream_window,
            local_connection_window_size: initial_connection_window,
            limits,
            http_limits,
            ..Self::default()
        };
        endpoint
            .header_codecs
            .as_mut()
            .expect("default endpoint owns HPACK codecs")
            .inbound
            .set_max_allowed_table_size(limits.max_header_table_size);
        Ok(endpoint)
    }

    pub(crate) fn with_local_flow_control_and_limits(
        initial_stream_window: u32,
        initial_connection_window: u32,
        limits: H2Limits,
    ) -> Result<Self, ServerError> {
        let http_limits = HttpLimits::new()
            .set_max_header_bytes(limits.max_header_list_size)
            .set_max_active_streams(limits.max_active_streams)
            .set_max_body_bytes(limits.max_queued_data_bytes);
        Self::with_local_flow_control_and_http_limits(
            initial_stream_window,
            initial_connection_window,
            limits,
            http_limits,
        )
    }

    pub(crate) const fn settings(&self) -> &H2Settings {
        &self.settings
    }

    pub(crate) fn inbound_hpack_diagnostics(&self) -> H2HpackDiagnosticsSnapshot {
        self.header_codecs
            .as_ref()
            .map_or_else(H2HpackDiagnosticsSnapshot::default, |codecs| {
                codecs.inbound.diagnostics().into()
            })
    }

    pub(crate) fn outbound_hpack_diagnostics(&self) -> H2HpackDiagnosticsSnapshot {
        self.header_codecs
            .as_ref()
            .map_or_else(H2HpackDiagnosticsSnapshot::default, |codecs| {
                codecs.outbound.diagnostics().into()
            })
    }

    pub(crate) fn next_outbound_block(&self) -> Option<H2OutboundBlockRef<'_>> {
        self.outbound_queue.front()
    }

    pub(crate) fn acknowledge_outbound_block(
        &mut self,
        commit: H2OutboundCommit,
    ) -> Result<(), ServerError> {
        self.outbound_queue.acknowledge(commit)
    }

    pub(crate) fn ensure_not_terminal(&self) -> Result<(), H2ProtocolError> {
        self.terminal_protocol_error.map_or(Ok(()), Err)
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_inbound_allocation_failure_after_for_testing(
        &mut self,
        successful_allocations: Option<usize>,
    ) {
        self.header_codecs
            .as_mut()
            .expect("test connection owns HPACK codecs")
            .inbound
            .set_allocation_failure_after(successful_allocations);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_outbound_allocation_failure_after_for_testing(
        &mut self,
        successful_allocations: Option<usize>,
    ) {
        self.outbound_queue
            .set_allocation_failure_after(successful_allocations);
        self.header_codecs
            .as_mut()
            .expect("test connection owns HPACK codecs")
            .outbound
            .set_allocation_failure_after(successful_allocations);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_assembly_allocation_failure_after_for_testing(
        &mut self,
        successful_allocations: Option<usize>,
    ) {
        self.header_block
            .set_allocation_failure_after(successful_allocations);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_pending_encoded_header_block_len_for_testing(&mut self, encoded_len: usize) {
        self.header_block.set_pending_encoded_len(encoded_len);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn inbound_table_for_testing(&self) -> crate::hpack::TestTableSnapshot {
        self.header_codecs
            .as_ref()
            .expect("test connection owns HPACK codecs")
            .inbound
            .test_table_snapshot()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn inbound_block_allocations_for_testing(
        &self,
    ) -> Option<crate::hpack::TestBlockAllocationSnapshot> {
        self.header_codecs
            .as_ref()
            .expect("test connection owns HPACK codecs")
            .inbound
            .test_block_allocations()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn outbound_table_for_testing(&self) -> crate::hpack::TestTableSnapshot {
        self.header_codecs
            .as_ref()
            .expect("test connection owns HPACK codecs")
            .outbound
            .test_table_snapshot()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn saturate_hpack_diagnostics_for_testing(&mut self) {
        let diagnostics = crate::hpack::Diagnostics {
            encoded_blocks: u64::MAX,
            decoded_blocks: u64::MAX,
            indexed_fields: u64::MAX,
            incremental_fields: u64::MAX,
            without_indexing_fields: u64::MAX,
            never_indexed_fields: u64::MAX,
            huffman_strings: u64::MAX,
            plain_strings: u64::MAX,
            table_size_updates: u64::MAX,
            table_insertions: u64::MAX,
            table_evictions: u64::MAX,
            compression_errors: u64::MAX,
            header_list_too_large: u64::MAX,
            field_bytes: u64::MAX,
            wire_bytes: u64::MAX,
        };
        let codecs = self
            .header_codecs
            .as_mut()
            .expect("test connection owns HPACK codecs");
        codecs.inbound.set_diagnostics_for_testing(diagnostics);
        codecs.outbound.set_diagnostics_for_testing(diagnostics);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn terminal_for_testing(&self) -> bool {
        self.terminal_protocol_error.is_some()
    }

    pub(crate) fn record_progress_frame(&mut self) {
        self.control_budget
            .refill_after_meaningful_progress(self.now);
    }

    pub(crate) fn record_control(&mut self, frame_type: H2FrameType) -> Result<(), ServerError> {
        let result = self.control_budget.record_control(frame_type);
        if result.is_err() {
            self.control_diagnostics.record_rejection(frame_type);
        }
        result
    }

    pub(crate) fn knows_stream(&self, stream_id: u32) -> bool {
        self.streams.contains_key(&stream_id) || self.tombstones.contains_key(&stream_id)
    }

    pub(crate) fn is_reset_tolerant(&self, stream_id: u32) -> bool {
        matches!(
            self.tombstones.get(&stream_id),
            Some(H2StreamTombstone::ResetTolerant)
        )
    }

    pub(crate) fn apply_send_window_update(
        &mut self,
        stream_id: u32,
        increment: u32,
        send_window: impl FnOnce(&mut Stream) -> Option<&mut H2FlowControlWindow>,
    ) -> Result<H2SendWindowUpdateDisposition, ServerError> {
        if stream_id == 0 {
            self.send_connection_window.increase(increment)?;
            return Ok(H2SendWindowUpdateDisposition::Applied);
        }
        if let Some(stream) = self.streams.get_mut(&stream_id)
            && let Some(window) = send_window(stream)
        {
            window.increase(increment)?;
            return Ok(H2SendWindowUpdateDisposition::Applied);
        }
        if self.knows_stream(stream_id) {
            Ok(H2SendWindowUpdateDisposition::IgnoredKnown)
        } else {
            Ok(H2SendWindowUpdateDisposition::Unknown)
        }
    }

    pub(crate) fn forget_stream(&mut self, stream_id: u32, kind: H2StreamTombstone) {
        self.streams.remove(&stream_id);
        self.remember_tombstone(stream_id, kind);
    }

    pub(crate) fn retire_if_complete(
        &mut self,
        stream_id: u32,
        kind: H2StreamTombstone,
        is_complete: impl FnOnce(&Stream) -> bool,
    ) {
        let Some(stream) = self.streams.get(&stream_id) else {
            return;
        };
        if is_complete(stream) {
            self.forget_stream(stream_id, kind);
        }
    }

    pub(crate) fn remember_tombstone(&mut self, stream_id: u32, kind: H2StreamTombstone) {
        if let Entry::Vacant(entry) = self.tombstones.entry(stream_id) {
            entry.insert(H2StreamTombstone::Closed);
            self.closed_stream_order.push_back(stream_id);
        }
        self.evict_tombstones();
        if kind == H2StreamTombstone::ResetTolerant && self.tombstones.contains_key(&stream_id) {
            self.tombstones
                .insert(stream_id, H2StreamTombstone::ResetTolerant);
        }
    }

    pub(crate) fn evict_tombstones(&mut self) {
        while self.tombstones.len() > self.limits.max_closed_stream_tombstones {
            if let Some(expired) = self.closed_stream_order.pop_front() {
                self.tombstones.remove(&expired);
            } else {
                break;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn assert_stream_bookkeeping(&self) {
        for stream_id in self.streams.keys() {
            assert!(
                !self.tombstones.contains_key(stream_id),
                "stream {stream_id} is both active and tombstoned"
            );
        }
        assert_eq!(
            self.tombstones.len(),
            self.closed_stream_order
                .iter()
                .filter(|stream_id| self.tombstones.contains_key(stream_id))
                .count(),
            "tombstone order drifted from the tombstone map"
        );
        assert!(
            self.tombstones.len() <= self.limits.max_closed_stream_tombstones,
            "tombstones exceeded the configured bound"
        );
    }

    pub(crate) fn mark_local_settings_sent(&mut self) {
        self.local_settings_ack_debt.note_sent();
    }

    pub(crate) const fn local_settings_state(&self) -> H2SettingsSyncState {
        self.local_settings_ack_debt.sync_state()
    }

    pub(crate) const fn control_diagnostics(&self) -> H2ControlDiagnostics {
        self.control_diagnostics
    }

    pub(crate) fn goaway_frame(
        &mut self,
        last_stream_id: u32,
        error_code: u32,
    ) -> Result<Vec<u8>, ServerError> {
        if let Some(previous) = self.outbound_shutdown.sent_last_stream_id()
            && last_stream_id > previous
        {
            return Err(ServerError::InvalidFrame);
        }
        self.outbound_shutdown = H2OutboundShutdown::GoawaySent { last_stream_id };
        self.control_diagnostics.goaways = self.control_diagnostics.goaways.saturating_add(1);
        let mut output = Vec::new();
        encode_h2_goaway(&mut output, last_stream_id, error_code);
        Ok(output)
    }

    pub(crate) fn enqueue_outbound_header_block(
        &mut self,
        stream_id: u32,
        fields: &[H2HeaderField],
        end_stream: bool,
        reserved_tail: usize,
        append_tail: impl FnOnce(&mut Vec<u8>),
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        enforce_h2_outbound_field_limits(
            stream_id,
            fields.len(),
            |index| fields[index].as_ref(),
            self.http_limits,
        )?;
        self.enqueue_outbound_header_block_by(
            stream_id,
            fields.len(),
            |index| fields[index].as_ref(),
            end_stream,
            reserved_tail,
            append_tail,
        )
    }

    pub(crate) fn enqueue_outbound_header_block_by<'a>(
        &mut self,
        stream_id: u32,
        field_count: usize,
        field_at: impl Fn(usize) -> H2RawHeaderRef<'a> + Copy,
        end_stream: bool,
        reserved_tail: usize,
        append_tail: impl FnOnce(&mut Vec<u8>),
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        self.ensure_not_terminal()?;
        let commit = self
            .outbound_queue
            .reserve_commit()
            .map_err(outbound_hpack_error)?;
        let encoder = &mut self
            .header_codecs
            .as_mut()
            .ok_or_else(|| outbound_hpack_error(H2HpackError::StateOverflow))?
            .outbound;
        let mut output = encode_connection_header_frames_by(
            encoder,
            stream_id,
            field_count,
            field_at,
            end_stream,
            self.settings.max_frame_size,
            reserved_tail,
        )
        .map_err(outbound_hpack_error)?;
        append_tail(&mut output);
        #[cfg(feature = "hpack-test-support")]
        {
            self.outbound_encoding_observations
                .queue_owned_frame_allocations = self
                .outbound_encoding_observations
                .queue_owned_frame_allocations
                .saturating_add(1);
        }
        self.outbound_queue.push_reserved(commit, output);
        Ok(commit)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn enqueue_prepared_request_header_block_by<'a>(
        &mut self,
        stream_id: u32,
        field_count: usize,
        field_at: impl Fn(usize) -> H2RawHeaderRef<'a> + Copy,
        end_stream: bool,
        preparation_limits: HttpLimits,
    ) -> Result<H2OutboundCommit, H2PreparedRequestError> {
        if let Err(error) = self.ensure_not_terminal() {
            validate_prepared_request_by(
                stream_id,
                field_count,
                field_at,
                preparation_limits,
                self.http_limits,
            )?;
            return Err(H2PreparedRequestError::Protocol(error));
        }
        let commit = match self.outbound_queue.reserve_commit() {
            Ok(commit) => commit,
            Err(error) => {
                validate_prepared_request_by(
                    stream_id,
                    field_count,
                    field_at,
                    preparation_limits,
                    self.http_limits,
                )?;
                return Err(H2PreparedRequestError::Protocol(outbound_hpack_error(
                    error,
                )));
            }
        };
        let Some(codecs) = self.header_codecs.as_mut() else {
            validate_prepared_request_by(
                stream_id,
                field_count,
                field_at,
                preparation_limits,
                self.http_limits,
            )?;
            return Err(H2PreparedRequestError::Protocol(outbound_hpack_error(
                H2HpackError::StateOverflow,
            )));
        };
        let output = encode_prepared_request_header_frames_by(
            &mut codecs.outbound,
            stream_id,
            field_count,
            field_at,
            end_stream,
            self.settings.max_frame_size,
            preparation_limits,
            self.http_limits,
        )?;
        #[cfg(feature = "hpack-test-support")]
        {
            self.outbound_encoding_observations
                .queue_owned_frame_allocations = self
                .outbound_encoding_observations
                .queue_owned_frame_allocations
                .saturating_add(1);
        }
        self.outbound_queue.push_reserved(commit, output);
        Ok(commit)
    }

    pub(crate) fn data_frame(&self, stream_id: u32, payload: &[u8], end_stream: bool) -> Vec<u8> {
        let mut output = Vec::new();
        let mut remaining = payload;
        while !remaining.is_empty() {
            let frame_len = remaining.len().min(self.settings.max_frame_size);
            let (chunk, rest) = remaining.split_at(frame_len);
            remaining = rest;
            H2Frame::encode_header(
                H2FrameType::Data,
                if end_stream && remaining.is_empty() {
                    0x1
                } else {
                    0
                },
                stream_id,
                chunk.len(),
                &mut output,
            );
            output.extend_from_slice(chunk);
        }
        if payload.is_empty() && end_stream {
            H2Frame {
                frame_type: H2FrameType::Data,
                flags: 0x1,
                stream_id,
                payload: Vec::new(),
            }
            .encode(&mut output);
        }
        output
    }

    pub(crate) fn connection_send_available(&self) -> usize {
        usize::try_from(self.send_connection_window.available()).unwrap_or(0)
    }

    pub(crate) fn consume_connection_window(&mut self, sent: usize) -> Result<(), ServerError> {
        self.send_connection_window.consume(sent)
    }

    pub(crate) fn trailers_frame(
        &mut self,
        stream_id: u32,
        headers: &[Header<'_>],
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        enforce_h2_outbound_field_limits(
            stream_id,
            headers.len(),
            |index| str_header_as_raw(&headers[index]),
            self.http_limits,
        )?;
        self.enqueue_outbound_header_block_by(
            stream_id,
            headers.len(),
            |index| str_header_as_raw(&headers[index]),
            true,
            0,
            |_| {},
        )
    }

    pub(crate) fn trailers_frame_with_raw_headers(
        &mut self,
        stream_id: u32,
        headers: &[H2HeaderField],
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        self.enqueue_outbound_header_block(stream_id, headers, true, 0, |_| {})
    }

    pub(crate) fn ensure_external_hpack_adapter_ready(&self) -> Result<(), ServerError> {
        if self.header_codecs.is_some() || !self.settings_seen {
            return Err(ServerError::InvalidFrame);
        }
        Ok(())
    }

    pub(crate) fn validate_complete_header_block(
        &mut self,
        block: &[u8],
    ) -> Result<(), ServerError> {
        if let Some(error) = self.terminal_protocol_error {
            return Err(error.into());
        }
        if !self.settings_seen {
            return Err(ServerError::InvalidFrame);
        }
        if block.len() > self.limits.max_encoded_header_block_size {
            let limit = self.limits.max_encoded_header_block_size;
            let actual = block.len();
            let error = encoded_header_limit_error(limit, actual);
            self.last_protocol_error = Some(error);
            self.terminal_protocol_error = Some(error);
            return Err(ServerError::HeaderTooLarge { limit, actual });
        }
        Ok(())
    }

    pub(crate) fn decode_h2_header_fields(
        &mut self,
        stream_id: u32,
        block: &[u8],
        role: H2HeaderValidationRole,
    ) -> Result<(Vec<H2HeaderField>, ValidatedSection), ServerError> {
        let decoded = decode_connection_header_fields(
            &mut self
                .header_codecs
                .as_mut()
                .ok_or(ServerError::InvalidFrame)?
                .inbound,
            &mut self.decoded_header_scratch,
            block,
            self.limits.max_header_list_size,
            stream_id,
            role,
            H2ConnectionHpackErrorState {
                last_hpack_error: &mut self.last_hpack_error,
                last_protocol_error: &mut self.last_protocol_error,
                terminal_protocol_error: &mut self.terminal_protocol_error,
            },
        );
        let section = match decoded {
            Ok(section) => section,
            Err(error) => {
                self.trim_decoded_header_scratch();
                return Err(error);
            }
        };
        let section = match section.enforce_limits(self.http_limits) {
            Ok(section) => section,
            Err(error) => {
                self.trim_decoded_header_scratch();
                return Err(error);
            }
        };
        let headers = std::mem::take(&mut self.decoded_header_scratch);
        Ok((headers, section))
    }

    pub(crate) fn decode_compact_h2_header_fields(
        &mut self,
        stream_id: u32,
        block: &[u8],
        role: H2HeaderValidationRole,
    ) -> Result<ValidatedSection, ServerError> {
        self.compact_header_scratch.set_retention_limits(
            self.http_limits.max_headers(),
            self.http_limits.max_header_bytes(),
        );
        decode_connection_header_fields(
            &mut self
                .header_codecs
                .as_mut()
                .ok_or(ServerError::InvalidFrame)?
                .inbound,
            &mut self.compact_header_scratch,
            block,
            self.limits.max_header_list_size,
            stream_id,
            role,
            H2ConnectionHpackErrorState {
                last_hpack_error: &mut self.last_hpack_error,
                last_protocol_error: &mut self.last_protocol_error,
                terminal_protocol_error: &mut self.terminal_protocol_error,
            },
        )?
        .enforce_limits(self.http_limits)
    }

    pub(crate) fn recycle_decoded_header_fields(&mut self, headers: Vec<H2HeaderField>) {
        if self.decoded_header_scratch.is_empty() {
            self.decoded_header_scratch = headers;
            self.trim_decoded_header_scratch();
        }
    }

    pub(crate) fn bind_compact_header_fields(
        &self,
        section: ValidatedSection,
        role: H2HeaderValidationRole,
    ) -> Result<ValidatedHeaderSectionRef<'_>, ServerError> {
        if section.role != role {
            return Err(ServerError::InvalidFrame);
        }
        let resolver = self
            .header_codecs
            .as_ref()
            .ok_or(ServerError::InvalidFrame)?
            .inbound
            .indexed_header_field_resolver();
        Ok(ValidatedHeaderSectionRef::new_compact(
            &self.compact_header_scratch,
            resolver,
            section,
        ))
    }

    pub(crate) fn compact_header_block(&self) -> Result<H2RawHeaderBlockRef<'_>, ServerError> {
        let resolver = self
            .header_codecs
            .as_ref()
            .ok_or(ServerError::InvalidFrame)?
            .inbound
            .indexed_header_field_resolver();
        Ok(H2RawHeaderBlockRef::new_compact(
            &self.compact_header_scratch,
            resolver,
        ))
    }

    pub(crate) fn reset_compact_header_fields(&mut self) {
        self.compact_header_scratch.reset();
    }

    fn trim_decoded_header_scratch(&mut self) {
        let retained_field_limit = self.http_limits.max_headers().min(100);
        let retained_byte_limit = self.http_limits.max_header_bytes().min(64 * 1024);
        let retained_bytes = self
            .decoded_header_scratch
            .iter()
            .fold(0usize, |total, field| {
                total
                    .saturating_add(field.name.capacity())
                    .saturating_add(field.value.capacity())
            });
        if self.decoded_header_scratch.capacity() > retained_field_limit
            || retained_bytes > retained_byte_limit
        {
            self.decoded_header_scratch = Vec::new();
        }
    }

    pub(crate) fn validate_external_header_fields(
        &mut self,
        stream_id: u32,
        role: H2HeaderValidationRole,
        headers: &[H2HeaderField],
    ) -> Result<ValidatedSection, ServerError> {
        let section = match validate_decoded_header_fields(headers, role) {
            Ok(section) => section,
            Err(validation) => {
                self.last_protocol_error = Some(H2ProtocolError::stream(
                    stream_id,
                    H2ErrorCode::ProtocolError,
                    "HTTP/2 header field validation failed",
                ));
                return Err(validation.into());
            }
        };
        section.enforce_limits(self.http_limits)
    }

    pub(crate) fn discard_hpack_block(&mut self, block: &[u8]) -> Result<(), ServerError> {
        if let Some(error) = self.terminal_protocol_error {
            return Err(error.into());
        }
        if block.len() > self.limits.max_encoded_header_block_size {
            let limit = self.limits.max_encoded_header_block_size;
            let actual = block.len();
            let error = encoded_header_limit_error(limit, actual);
            self.last_protocol_error = Some(error);
            self.terminal_protocol_error = Some(error);
            return Err(ServerError::HeaderTooLarge { limit, actual });
        }
        let Some(codecs) = self.header_codecs.as_mut() else {
            return Err(ServerError::InvalidFrame);
        };
        match codecs
            .inbound
            .decode(block, self.limits.max_header_list_size)
        {
            Ok(_) => Ok(()),
            Err(crate::hpack::Error::HeaderListTooLarge { actual }) => {
                let limit = self.limits.max_header_list_size;
                self.last_protocol_error = Some(decoded_header_limit_error(
                    H2ErrorScope::Connection,
                    limit,
                    actual,
                ));
                Err(ServerError::HeaderTooLarge { limit, actual })
            }
            Err(crate::hpack::Error::AllocationFailed) => {
                let error = allocation_terminal_error();
                self.last_protocol_error = Some(error);
                self.terminal_protocol_error = Some(error);
                Err(ServerError::InvalidFrame)
            }
            Err(error) => {
                self.last_hpack_error = Some(error.into());
                self.terminal_protocol_error = Some(poisoned_connection_error());
                Err(ServerError::InvalidHpack)
            }
        }
    }

    pub(crate) fn prepare_settings(
        &mut self,
        payload: &[u8],
    ) -> Result<H2PreparedSettings, ServerError> {
        let previous_initial_window_size = self.settings.initial_window_size;
        let decoded =
            H2Settings::decode_payload_with_limit(payload, self.limits.max_settings_entries)?;
        if decoded.iter().any(|setting| {
            setting.id == H2SettingId::InitialWindowSize && setting.value > H2_MAX_WINDOW_SIZE
        }) {
            self.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::FlowControlError,
                "SETTINGS_INITIAL_WINDOW_SIZE exceeds the HTTP/2 window maximum",
            ));
            return Err(ServerError::InvalidFrame);
        }
        let mut settings = self.settings;
        settings.apply_all(&decoded)?;
        let table_sizes = decoded
            .iter()
            .filter(|setting| setting.id == H2SettingId::HeaderTableSize)
            .map(|setting| setting.value as usize)
            .collect();
        Ok(H2PreparedSettings {
            settings,
            previous_initial_window_size,
            table_sizes,
        })
    }

    pub(crate) fn commit_settings(
        &mut self,
        prepared: H2PreparedSettings,
    ) -> Option<H2InitialWindowSizeChange> {
        let change = prepared.initial_window_change();
        if let Some(codecs) = self.header_codecs.as_mut() {
            for size in prepared.table_sizes {
                codecs.outbound.set_max_table_size(size);
            }
        }
        self.settings = prepared.settings;
        change
    }

    pub(crate) fn adjust_send_windows(
        &mut self,
        delta: i32,
        send_window: impl for<'a> Fn(&'a mut Stream) -> Option<&'a mut H2FlowControlWindow> + Copy,
    ) -> Result<(), ServerError> {
        for stream in self.streams.values_mut() {
            let Some(window) = send_window(stream) else {
                continue;
            };
            let mut adjusted = *window;
            if adjusted.adjust(delta).is_err() {
                self.last_protocol_error = Some(H2ProtocolError::connection(
                    H2ErrorCode::FlowControlError,
                    "SETTINGS_INITIAL_WINDOW_SIZE overflowed an active send window",
                ));
                return Err(ServerError::FlowControlViolation);
            }
        }
        for stream in self.streams.values_mut() {
            if let Some(window) = send_window(stream) {
                window.adjust(delta)?;
            }
        }
        Ok(())
    }

    pub(crate) fn apply_settings(
        &mut self,
        payload: &[u8],
        send_window: impl for<'a> Fn(&'a mut Stream) -> Option<&'a mut H2FlowControlWindow> + Copy,
    ) -> Result<Option<H2InitialWindowSizeChange>, ServerError> {
        let prepared = self.prepare_settings(payload)?;
        if let Some(delta) = prepared.initial_window_delta()? {
            self.adjust_send_windows(delta, send_window)?;
        }
        Ok(self.commit_settings(prepared))
    }

    pub(crate) fn encode_local_settings(&mut self, output: &mut Vec<u8>) {
        self.local_settings_ack_debt.note_sent();
        let mut payload = Vec::new();
        let mut settings = Vec::new();
        if let Some(enabled) = self.local_enable_push {
            settings.push(H2Setting::new(H2SettingId::EnablePush, u32::from(enabled)));
        }
        if self.limits.max_header_table_size != H2Settings::default().header_table_size as usize {
            settings.push(H2Setting::new(
                H2SettingId::HeaderTableSize,
                self.limits
                    .max_header_table_size
                    .min(crate::hpack::MAX_TABLE_SIZE) as u32,
            ));
        }
        if self.local_initial_window_size != H2Settings::default().initial_window_size {
            settings.push(H2Setting::new(
                H2SettingId::InitialWindowSize,
                self.local_initial_window_size,
            ));
        }
        if self.limits.max_active_streams != usize::MAX {
            settings.push(H2Setting::new(
                H2SettingId::MaxConcurrentStreams,
                self.limits.max_active_streams.min(u32::MAX as usize) as u32,
            ));
        }
        if self.limits.max_header_list_size != usize::MAX {
            settings.push(H2Setting::new(
                H2SettingId::MaxHeaderListSize,
                self.limits.max_header_list_size.min(u32::MAX as usize) as u32,
            ));
        }
        H2Settings::encode_payload(&settings, &mut payload);
        H2Frame {
            frame_type: H2FrameType::Settings,
            flags: 0,
            stream_id: 0,
            payload,
        }
        .encode(output);
    }

    pub(crate) fn encode_local_connection_window_update(&self, output: &mut Vec<u8>) {
        let default = H2Settings::default().initial_window_size;
        let Some(increment) = self.local_connection_window_size.checked_sub(default) else {
            return;
        };
        if increment == 0 {
            return;
        }
        H2Frame {
            frame_type: H2FrameType::WindowUpdate,
            flags: 0,
            stream_id: 0,
            payload: increment.to_be_bytes().to_vec(),
        }
        .encode(output);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct H2SentBodyState {
    pub(crate) sent: usize,
    pub(crate) limit: Option<usize>,
}

impl H2SentBodyState {
    pub(crate) fn new(limit: Option<usize>) -> Self {
        Self { sent: 0, limit }
    }

    pub(crate) fn validate_pending(self, pending: usize) -> Result<(), ServerError> {
        enforce_optional_body_size(self.sent.saturating_add(pending), self.limit)
    }

    pub(crate) fn account(&mut self, sent: usize) -> Result<(), ServerError> {
        self.sent = self.sent.saturating_add(sent);
        enforce_optional_body_size(self.sent, self.limit)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct H2OutboundHalf {
    pub(crate) window: H2FlowControlWindow,
    pub(crate) sent: H2SentBodyState,
}

impl H2OutboundHalf {
    pub(crate) fn new(initial_window: u32, body_limit: Option<usize>) -> Self {
        Self {
            window: H2FlowControlWindow::new(initial_window)
                .expect("validated peer initial window"),
            sent: H2SentBodyState::new(body_limit),
        }
    }

    pub(crate) fn consume_window(&mut self, sent: usize) -> Result<(), ServerError> {
        self.window.consume(sent)
    }

    pub(crate) fn account_body(&mut self, sent: usize) -> Result<(), ServerError> {
        self.sent.account(sent)
    }
}

pub(crate) fn h2_send_capacity(
    half: &H2OutboundHalf,
    connection_available: usize,
    stream_id: u32,
    pending_bytes: usize,
) -> H2SendCapacity {
    H2SendCapacity::new(
        stream_id,
        pending_bytes,
        half.window.available(),
        connection_available,
    )
}

pub(crate) fn h2_plan_data_frame(
    half: &H2OutboundHalf,
    connection_available: usize,
    max_frame_size: usize,
    stream_id: u32,
    pending_bytes: usize,
    end_stream: bool,
    allow_empty_nonterminal: bool,
) -> Result<Option<H2DataFramePlan>, ServerError> {
    half.sent.validate_pending(pending_bytes)?;
    let capacity = h2_send_capacity(half, connection_available, stream_id, pending_bytes);
    if pending_bytes != 0 && capacity.sendable_bytes == 0 {
        return Ok(None);
    }
    if pending_bytes == 0 && !end_stream && !allow_empty_nonterminal {
        return Ok(None);
    }
    let payload_len = capacity.sendable_bytes.min(max_frame_size);
    Ok(Some(h2_data_frame_plan(
        stream_id,
        payload_len,
        end_stream && payload_len == pending_bytes,
    )))
}

pub(crate) fn h2_validate_data_plan(
    half: &H2OutboundHalf,
    connection_available: usize,
    max_frame_size: usize,
    plan: H2DataFramePlan,
) -> Result<(), ServerError> {
    let capacity = h2_send_capacity(half, connection_available, plan.stream_id, plan.payload_len);
    if capacity.sendable_bytes != plan.payload_len || plan.payload_len > max_frame_size {
        return Err(ServerError::FlowControlViolation);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum H2StreamTombstone {
    Closed,
    ResetTolerant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum H2SendWindowUpdateDisposition {
    Applied,
    IgnoredKnown,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct H2StreamState {
    pub(crate) content_length: Option<usize>,
    pub(crate) received_data_len: usize,
    pub(crate) body_limit: Option<usize>,
    pub(crate) data_forbidden: bool,
    pub(crate) header_count: usize,
    pub(crate) header_bytes: usize,
}

pub(crate) enum H2HeaderStreamDisposition {
    Accept,
    Refuse,
}

impl H2StreamState {
    pub(crate) fn new_raw(
        section: ValidatedSection,
        limits: HttpLimits,
        body_limit: Option<usize>,
    ) -> Result<Self, ServerError> {
        let section = section.enforce_limits(limits)?;
        let content_length = section.content_length;
        enforce_optional_body_size(content_length.unwrap_or(0), body_limit)?;
        Ok(Self {
            content_length,
            received_data_len: 0,
            body_limit,
            data_forbidden: false,
            header_count: section.field_count,
            header_bytes: section.field_bytes,
        })
    }

    pub(crate) fn receive_data(
        &mut self,
        amount: usize,
        end_stream: bool,
    ) -> Result<(), ServerError> {
        if self.data_forbidden {
            return Err(ServerError::InvalidFrame);
        }
        self.received_data_len =
            self.received_data_len
                .checked_add(amount)
                .ok_or(ServerError::BodyTooLarge {
                    limit: self.body_limit.unwrap_or(usize::MAX),
                    actual: usize::MAX,
                })?;
        enforce_optional_body_size(self.received_data_len, self.body_limit)?;
        if let Some(expected) = self.content_length {
            if self.received_data_len > expected {
                return Err(ServerError::InvalidContentLength);
            }
            if end_stream && self.received_data_len != expected {
                return Err(ServerError::InvalidContentLength);
            }
        }
        Ok(())
    }

    pub(crate) fn new_response_raw(
        section: ValidatedSection,
        request_is_head: bool,
        status: u16,
        limits: HttpLimits,
        body_limit: Option<usize>,
    ) -> Result<Self, ServerError> {
        let section = section.enforce_limits(limits)?;
        let content_length = section.content_length;
        if status == 204 && content_length.is_some() {
            return Err(ServerError::InvalidContentLength);
        }
        if status == 205 && content_length.is_some_and(|length| length != 0) {
            return Err(ServerError::InvalidContentLength);
        }
        let data_forbidden = request_is_head || matches!(status, 204 | 205 | 304);
        if !data_forbidden {
            enforce_optional_body_size(content_length.unwrap_or(0), body_limit)?;
        }
        Ok(Self {
            content_length: if data_forbidden { None } else { content_length },
            received_data_len: 0,
            body_limit,
            data_forbidden,
            header_count: section.field_count,
            header_bytes: section.field_bytes,
        })
    }

    pub(crate) fn accept_trailers(
        &mut self,
        section: ValidatedSection,
        limits: HttpLimits,
    ) -> Result<(), ServerError> {
        let actual_count = self.header_count.saturating_add(section.field_count);
        if actual_count > limits.max_headers() {
            return Err(ServerError::TooManyHeaders {
                limit: limits.max_headers(),
                actual: actual_count,
            });
        }
        let actual_bytes = self.header_bytes.saturating_add(section.field_bytes);
        if actual_bytes > limits.max_header_bytes() {
            return Err(ServerError::HeaderTooLarge {
                limit: limits.max_header_bytes(),
                actual: actual_bytes,
            });
        }
        self.header_count = actual_count;
        self.header_bytes = actual_bytes;
        Ok(())
    }

    pub(crate) fn finish(&self) -> Result<(), ServerError> {
        if let Some(expected) = self.content_length
            && self.received_data_len != expected
        {
            return Err(ServerError::InvalidContentLength);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct H2HeaderBlockAssembler {
    pub(crate) pending: Option<H2PendingHeaderBlock>,
    #[cfg(feature = "hpack-test-support")]
    pub(crate) allocation_failure_after: Option<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct H2PendingHeaderBlock {
    pub(crate) stream_id: u32,
    pub(crate) flags: u8,
    pub(crate) block: Vec<u8>,
    pub(crate) encoded_len: usize,
    pub(crate) continuation_frames: usize,
    pub(crate) self_dependency: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct H2CompleteHeaderBlock<'a> {
    pub(crate) stream_id: u32,
    pub(crate) flags: u8,
    pub(crate) block: Cow<'a, [u8]>,
    pub(crate) self_dependency: bool,
}

pub(crate) enum H2HeaderBlockAssemblyError {
    Protocol(ServerError),
    EncodedLimit(usize),
    Allocation,
}

impl From<ServerError> for H2HeaderBlockAssemblyError {
    fn from(error: ServerError) -> Self {
        Self::Protocol(error)
    }
}

impl H2HeaderBlockAssembler {
    pub(crate) fn accept<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
        limits: H2Limits,
    ) -> Result<Option<H2CompleteHeaderBlock<'a>>, H2HeaderBlockAssemblyError> {
        match frame.frame_type {
            H2FrameType::Headers => self.accept_headers(frame, limits),
            H2FrameType::Continuation => self.accept_continuation(frame, limits),
            _ if self.pending.is_some() => Err(H2HeaderBlockAssemblyError::Protocol(
                ServerError::InvalidFrame,
            )),
            _ => Ok(None),
        }
    }

    pub(crate) fn accept_headers<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
        limits: H2Limits,
    ) -> Result<Option<H2CompleteHeaderBlock<'a>>, H2HeaderBlockAssemblyError> {
        if self.pending.is_some() || frame.stream_id == 0 {
            return Err(H2HeaderBlockAssemblyError::Protocol(
                ServerError::InvalidFrame,
            ));
        }
        let self_dependency = header_has_self_dependency(frame);
        let block = headers_payload(frame.stream_id, frame.flags, frame.payload)
            .map_err(H2HeaderBlockAssemblyError::Protocol)?;
        self.check_header_block_len(block.len(), limits)?;
        if frame.flags & 0x4 != 0 {
            return Ok(Some(H2CompleteHeaderBlock {
                stream_id: frame.stream_id,
                flags: frame.flags,
                block: Cow::Borrowed(block),
                self_dependency,
            }));
        }
        let mut owned = Vec::new();
        self.try_reserve(&mut owned, block.len())?;
        owned.extend_from_slice(block);
        self.pending = Some(H2PendingHeaderBlock {
            stream_id: frame.stream_id,
            flags: frame.flags,
            block: owned,
            encoded_len: block.len(),
            continuation_frames: 0,
            self_dependency,
        });
        Ok(None)
    }

    pub(crate) fn accept_continuation<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
        limits: H2Limits,
    ) -> Result<Option<H2CompleteHeaderBlock<'a>>, H2HeaderBlockAssemblyError> {
        let Some(mut pending) = self.pending.take() else {
            return Err(H2HeaderBlockAssemblyError::Protocol(
                ServerError::InvalidFrame,
            ));
        };
        if frame.stream_id != pending.stream_id {
            self.pending = Some(pending);
            return Err(H2HeaderBlockAssemblyError::Protocol(
                ServerError::InvalidFrame,
            ));
        }
        pending.continuation_frames = pending.continuation_frames.checked_add(1).ok_or(
            H2HeaderBlockAssemblyError::Protocol(ServerError::InvalidFrame),
        )?;
        if pending.continuation_frames > limits.max_continuation_frames {
            self.pending = Some(pending);
            return Err(H2HeaderBlockAssemblyError::Protocol(
                ServerError::InvalidFrame,
            ));
        }
        let next_len = pending
            .encoded_len
            .checked_add(frame.payload.len())
            .ok_or(H2HeaderBlockAssemblyError::EncodedLimit(usize::MAX))?;
        self.check_header_block_len(next_len, limits)?;
        self.try_reserve(&mut pending.block, frame.payload.len())?;
        pending.block.extend_from_slice(frame.payload);
        pending.encoded_len = next_len;
        if frame.flags & 0x4 != 0 {
            Ok(Some(H2CompleteHeaderBlock {
                stream_id: pending.stream_id,
                flags: pending.flags | 0x4,
                block: Cow::Owned(pending.block),
                self_dependency: pending.self_dependency,
            }))
        } else {
            self.pending = Some(pending);
            Ok(None)
        }
    }

    pub(crate) fn check_header_block_len(
        &self,
        len: usize,
        limits: H2Limits,
    ) -> Result<(), H2HeaderBlockAssemblyError> {
        if len > limits.max_encoded_header_block_size {
            Err(H2HeaderBlockAssemblyError::EncodedLimit(len))
        } else {
            Ok(())
        }
    }

    pub(crate) fn try_reserve(
        &mut self,
        block: &mut Vec<u8>,
        additional: usize,
    ) -> Result<(), H2HeaderBlockAssemblyError> {
        if additional <= block.capacity().saturating_sub(block.len()) {
            return Ok(());
        }
        #[cfg(feature = "hpack-test-support")]
        if let Some(remaining) = self.allocation_failure_after.as_mut() {
            if *remaining == 0 {
                return Err(H2HeaderBlockAssemblyError::Allocation);
            }
            *remaining -= 1;
        }
        block
            .try_reserve(additional)
            .map_err(|_| H2HeaderBlockAssemblyError::Allocation)
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_allocation_failure_after(&mut self, successful_allocations: Option<usize>) {
        self.allocation_failure_after = successful_allocations;
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_pending_encoded_len(&mut self, encoded_len: usize) {
        self.pending
            .as_mut()
            .expect("test must first create pending header assembly")
            .encoded_len = encoded_len;
    }
}

pub(crate) fn accept_header_frame_for_connection<'a>(
    assembler: &mut H2HeaderBlockAssembler,
    frame: H2FrameRef<'a>,
    limits: H2Limits,
    last_protocol_error: &mut Option<H2ProtocolError>,
    terminal_protocol_error: &mut Option<H2ProtocolError>,
) -> Result<Option<H2CompleteHeaderBlock<'a>>, ServerError> {
    match assembler.accept(frame, limits) {
        Ok(block) => Ok(block),
        Err(H2HeaderBlockAssemblyError::Protocol(error)) => {
            *last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "invalid HTTP/2 header-block framing",
            ));
            Err(error)
        }
        Err(H2HeaderBlockAssemblyError::EncodedLimit(actual)) => {
            assembler.pending = None;
            let limit = limits.max_encoded_header_block_size;
            let error = encoded_header_limit_error(limit, actual);
            *last_protocol_error = Some(error);
            *terminal_protocol_error = Some(error);
            Err(ServerError::HeaderTooLarge { limit, actual })
        }
        Err(H2HeaderBlockAssemblyError::Allocation) => {
            assembler.pending = None;
            let error = allocation_terminal_error();
            *last_protocol_error = Some(error);
            *terminal_protocol_error = Some(error);
            Err(ServerError::InvalidFrame)
        }
    }
}

pub(crate) fn header_has_self_dependency(frame: H2FrameRef<'_>) -> bool {
    if frame.frame_type != H2FrameType::Headers || frame.flags & 0x20 == 0 {
        return false;
    }
    let Ok(payload) = strip_padding(frame.flags, frame.payload) else {
        return false;
    };
    payload.len() >= 5
        && (u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) & 0x7fff_ffff)
            == frame.stream_id
}

#[derive(Clone, Debug)]
pub(crate) struct H2ControlFrameBudget {
    pub(crate) settings: u32,
    pub(crate) window_update: u32,
    pub(crate) ping: u32,
    pub(crate) reset: u32,
    pub(crate) goaway: u32,
    pub(crate) priority: u32,
    pub(crate) last_refill: Duration,
}

pub(crate) const H2_CONTROL_BUDGET_REFILL_INTERVAL: Duration = Duration::from_secs(1);
// A peer can legitimately return one stream and one connection WINDOW_UPDATE
// for each validated protocol-progress frame.
pub(crate) const H2_WINDOW_UPDATE_CREDITS_PER_PROGRESS_FRAME: u32 = 2;

#[derive(Clone, Copy)]
pub(crate) struct H2ControlFrameBudgetLimits {
    pub(crate) settings: u32,
    pub(crate) window_update: u32,
    pub(crate) ping: u32,
    pub(crate) reset: u32,
    pub(crate) goaway: u32,
    pub(crate) priority: u32,
}

pub(crate) const H2_CONTROL_FRAME_BUDGET_LIMITS: H2ControlFrameBudgetLimits =
    H2ControlFrameBudgetLimits {
        settings: 64,
        window_update: 4096,
        ping: 64,
        reset: 256,
        goaway: 4,
        priority: 256,
    };

impl Default for H2ControlFrameBudget {
    fn default() -> Self {
        Self {
            settings: H2_CONTROL_FRAME_BUDGET_LIMITS.settings,
            window_update: H2_CONTROL_FRAME_BUDGET_LIMITS.window_update,
            ping: H2_CONTROL_FRAME_BUDGET_LIMITS.ping,
            reset: H2_CONTROL_FRAME_BUDGET_LIMITS.reset,
            goaway: H2_CONTROL_FRAME_BUDGET_LIMITS.goaway,
            priority: H2_CONTROL_FRAME_BUDGET_LIMITS.priority,
            last_refill: Duration::ZERO,
        }
    }
}

impl H2ControlFrameBudget {
    pub(crate) fn record_control(&mut self, frame_type: H2FrameType) -> Result<(), ServerError> {
        self.record_control_without_refill(frame_type)
    }

    pub(crate) fn record_control_without_refill(
        &mut self,
        frame_type: H2FrameType,
    ) -> Result<(), ServerError> {
        let remaining = match frame_type {
            H2FrameType::Settings => &mut self.settings,
            H2FrameType::WindowUpdate => &mut self.window_update,
            H2FrameType::Ping => &mut self.ping,
            H2FrameType::RstStream => &mut self.reset,
            H2FrameType::Goaway => &mut self.goaway,
            H2FrameType::Priority => &mut self.priority,
            H2FrameType::Data
            | H2FrameType::Headers
            | H2FrameType::Continuation
            | H2FrameType::PushPromise
            | H2FrameType::Unknown(_) => return Ok(()),
        };
        let Some(next) = remaining.checked_sub(1) else {
            return Err(ServerError::InvalidFrame);
        };
        *remaining = next;
        Ok(())
    }

    pub(crate) fn refill_after_meaningful_progress(&mut self, now: Duration) {
        self.refill_if_due(now);
        self.window_update = self
            .window_update
            .saturating_add(H2_WINDOW_UPDATE_CREDITS_PER_PROGRESS_FRAME)
            .min(H2_CONTROL_FRAME_BUDGET_LIMITS.window_update);
    }

    pub(crate) fn refill_if_due(&mut self, now: Duration) {
        if now.saturating_sub(self.last_refill) >= H2_CONTROL_BUDGET_REFILL_INTERVAL {
            self.settings = H2_CONTROL_FRAME_BUDGET_LIMITS.settings;
            self.window_update = H2_CONTROL_FRAME_BUDGET_LIMITS.window_update;
            self.ping = H2_CONTROL_FRAME_BUDGET_LIMITS.ping;
            self.reset = H2_CONTROL_FRAME_BUDGET_LIMITS.reset;
            self.goaway = H2_CONTROL_FRAME_BUDGET_LIMITS.goaway;
            self.priority = H2_CONTROL_FRAME_BUDGET_LIMITS.priority;
            self.last_refill = now;
        }
    }

    #[cfg(test)]
    pub(crate) fn record_control_at(
        &mut self,
        frame_type: H2FrameType,
        now: Duration,
    ) -> Result<(), ServerError> {
        self.refill_if_due(now);
        self.record_control_without_refill(frame_type)
    }
}

#[cfg(test)]
mod decoded_header_scratch_tests {
    use super::*;

    #[test]
    fn excessive_recycled_field_capacity_is_not_retained() {
        let mut endpoint = H2Endpoint::<()>::default();
        let mut headers = Vec::with_capacity(101);
        headers.push(H2HeaderField {
            name: Vec::new(),
            value: Vec::new(),
            sensitive: true,
        });

        endpoint.recycle_decoded_header_fields(headers);

        assert!(endpoint.decoded_header_scratch.is_empty());
        assert_eq!(endpoint.decoded_header_scratch.capacity(), 0);
    }

    #[test]
    fn excessive_recycled_byte_capacity_is_not_retained() {
        let mut endpoint = H2Endpoint::<()>::default();
        let headers = vec![H2HeaderField {
            name: Vec::with_capacity(64 * 1024 + 1),
            value: Vec::new(),
            sensitive: false,
        }];

        endpoint.recycle_decoded_header_fields(headers);

        assert!(endpoint.decoded_header_scratch.is_empty());
        assert_eq!(endpoint.decoded_header_scratch.capacity(), 0);
    }

    #[test]
    fn small_fields_are_retained_after_excess_capacity_is_dropped() {
        let mut endpoint = H2Endpoint::<()>::default();
        endpoint.recycle_decoded_header_fields(Vec::with_capacity(101));
        assert_eq!(endpoint.decoded_header_scratch.capacity(), 0);

        endpoint.recycle_decoded_header_fields(vec![H2HeaderField {
            name: b":method".to_vec(),
            value: b"GET".to_vec(),
            sensitive: false,
        }]);

        assert_eq!(endpoint.decoded_header_scratch.len(), 1);
        assert!(endpoint.decoded_header_scratch.capacity() >= 1);
    }
}
