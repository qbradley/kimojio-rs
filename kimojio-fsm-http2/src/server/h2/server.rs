//! HTTP/2 server connection state machine.

use crate::Header;
use crate::HttpLimits;
use crate::server::ServerError;
use crate::server::h2::endpoint::*;
use crate::server::h2::events::*;
use crate::server::h2::flow::*;
use crate::server::h2::headers::*;
use crate::server::h2::wire::*;
use crate::server::util::decimal_bytes;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct H2ServerStream {
    pub(crate) inbound: Option<H2StreamState>,
    pub(crate) outbound: Option<H2OutboundHalf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum H2ServerFrameOutput {
    Emit,
    Suppress,
}

pub struct H2Server {
    pub(crate) endpoint: H2Endpoint<H2ServerStream>,
    pub(crate) preface_seen: bool,
    pub(crate) max_peer_stream_id: u32,
    pub(crate) max_processed_peer_stream_id: u32,
    pub(crate) request_body_limit: Option<usize>,
    pub(crate) reported_protocol_error: Option<H2ProtocolError>,
    pub(crate) last_compat_error_recoverable: bool,
    pub(crate) handled_compat_error: Option<ServerError>,
    pub(crate) handled_stream_error: Option<H2ProtocolError>,
}

pub(crate) struct H2ServerDriverProgress<'a> {
    pub(crate) event: Option<H2DriverServerEvent<'a>>,
    pub(crate) consumed: usize,
    pub(crate) output: Vec<u8>,
    pub(crate) handled_protocol_error: Option<H2ProtocolError>,
    pub(crate) handled_compat_error: Option<ServerError>,
}

impl Default for H2Server {
    fn default() -> Self {
        Self::from_endpoint(H2Endpoint::default())
    }
}

impl H2Server {
    fn from_endpoint(endpoint: H2Endpoint<H2ServerStream>) -> Self {
        let request_body_limit = Some(endpoint.http_limits.max_body_bytes());
        Self {
            endpoint,
            preface_seen: false,
            max_peer_stream_id: 0,
            max_processed_peer_stream_id: 0,
            request_body_limit,
            reported_protocol_error: None,
            last_compat_error_recoverable: false,
            handled_compat_error: None,
            handled_stream_error: None,
        }
    }
}

impl H2Server {
    /// Creates protocol state for an adapter that owns the connection HPACK pair.
    ///
    /// Header blocks must be decoded by the adapter and supplied through
    /// [`Self::accept_external_header_fields`]. This prevents a second active
    /// codec history in layered connection implementations.
    pub fn for_external_hpack_adapter() -> Self {
        Self::from_endpoint(H2Endpoint::for_external_hpack_adapter())
    }

    pub fn with_local_flow_control(
        initial_stream_window: u32,
        initial_connection_window: u32,
    ) -> Result<Self, ServerError> {
        Self::with_local_flow_control_and_limits(
            initial_stream_window,
            initial_connection_window,
            H2Limits::default(),
        )
    }

    pub fn with_local_flow_control_and_limits(
        initial_stream_window: u32,
        initial_connection_window: u32,
        limits: H2Limits,
    ) -> Result<Self, ServerError> {
        H2Endpoint::with_local_flow_control_and_limits(
            initial_stream_window,
            initial_connection_window,
            limits,
        )
        .map(Self::from_endpoint)
    }

    /// Creates protocol state with explicit HTTP and HTTP/2 resource limits.
    pub fn with_local_flow_control_and_http_limits(
        initial_stream_window: u32,
        initial_connection_window: u32,
        limits: H2Limits,
        http_limits: HttpLimits,
    ) -> Result<Self, ServerError> {
        H2Endpoint::with_local_flow_control_and_http_limits(
            initial_stream_window,
            initial_connection_window,
            limits,
            http_limits,
        )
        .map(Self::from_endpoint)
    }

    pub fn with_limits(limits: H2Limits) -> Result<Self, ServerError> {
        Self::with_local_flow_control_and_limits(
            H2Settings::default().initial_window_size,
            H2Settings::default().initial_window_size,
            limits,
        )
    }

    pub const fn settings(&self) -> &H2Settings {
        self.endpoint.settings()
    }

    pub(crate) fn stream_request_bodies(&mut self) {
        debug_assert!(
            self.endpoint
                .streams
                .values()
                .all(|stream| stream.inbound.is_none())
        );
        self.request_body_limit = None;
    }

    pub(crate) fn stream_response_body(&mut self, stream_id: u32) -> Result<(), ServerError> {
        let body = self
            .outbound_mut(stream_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        body.sent.limit = None;
        Ok(())
    }

    pub fn inbound_hpack_diagnostics(&self) -> H2HpackDiagnosticsSnapshot {
        self.endpoint.inbound_hpack_diagnostics()
    }

    pub(crate) const fn client_preface_seen(&self) -> bool {
        self.preface_seen
    }

    pub fn outbound_hpack_diagnostics(&self) -> H2HpackDiagnosticsSnapshot {
        self.endpoint.outbound_hpack_diagnostics()
    }

    /// Returns the next complete outbound transaction in connection wire order.
    ///
    /// Check that the FIFO front has the expected receipt before assigning its
    /// bytes. Leave the block unacknowledged if assignment fails.
    ///
    pub fn next_outbound_block(&self) -> Option<H2OutboundBlockRef<'_>> {
        self.endpoint.next_outbound_block()
    }

    /// Acknowledges that the next complete outbound transaction was handed to the transport.
    pub fn acknowledge_outbound_block(
        &mut self,
        commit: H2OutboundCommit,
    ) -> Result<(), ServerError> {
        self.endpoint.acknowledge_outbound_block(commit)
    }

    pub(crate) fn defer_response_end_stream(
        &mut self,
        commit: H2OutboundCommit,
    ) -> Result<(), ServerError> {
        // RFC 9113 section 8.1 makes a trailing HEADERS block the stream
        // terminator, so a queued bodyless response head must surrender
        // END_STREAM before the transport can observe it.
        self.endpoint
            .outbound_queue
            .clear_headers_end_stream(commit)
    }

    /// Begins a reversible raw header-block transaction.
    pub fn prepare_outbound_header_block<'connection, 'headers>(
        &'connection mut self,
        stream_id: u32,
        headers: &'headers [H2HeaderField],
        end_stream: bool,
    ) -> Result<H2OutboundHeaderBlock<'connection, 'headers>, H2ProtocolError> {
        self.endpoint.ensure_not_terminal()?;
        Ok(H2OutboundHeaderBlock {
            target: H2OutboundHeaderBlockTarget::Server(self),
            stream_id,
            headers,
            end_stream,
        })
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_inbound_allocation_failure_after_for_testing(
        &mut self,
        successful_allocations: Option<usize>,
    ) {
        self.endpoint
            .set_inbound_allocation_failure_after_for_testing(successful_allocations);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn inbound_block_allocations_for_testing(
        &self,
    ) -> Option<crate::hpack::TestBlockAllocationSnapshot> {
        self.endpoint.inbound_block_allocations_for_testing()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_outbound_allocation_failure_after_for_testing(
        &mut self,
        successful_allocations: Option<usize>,
    ) {
        self.endpoint
            .set_outbound_allocation_failure_after_for_testing(successful_allocations);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_assembly_allocation_failure_after_for_testing(
        &mut self,
        successful_allocations: Option<usize>,
    ) {
        self.endpoint
            .set_assembly_allocation_failure_after_for_testing(successful_allocations);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_pending_encoded_header_block_len_for_testing(&mut self, encoded_len: usize) {
        self.endpoint
            .set_pending_encoded_header_block_len_for_testing(encoded_len);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn inbound_table_for_testing(&self) -> crate::hpack::TestTableSnapshot {
        self.endpoint.inbound_table_for_testing()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn outbound_table_for_testing(&self) -> crate::hpack::TestTableSnapshot {
        self.endpoint.outbound_table_for_testing()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn saturate_hpack_diagnostics_for_testing(&mut self) {
        self.endpoint.saturate_hpack_diagnostics_for_testing();
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn terminal_for_testing(&self) -> bool {
        self.endpoint.terminal_for_testing()
    }

    pub fn record_progress_frame(&mut self) {
        self.endpoint.record_progress_frame();
    }

    pub(crate) fn record_control(&mut self, frame_type: H2FrameType) -> Result<(), ServerError> {
        match self.endpoint.record_control(frame_type) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.endpoint.last_protocol_error = Some(H2ProtocolError::connection(
                    H2ErrorCode::EnhanceYourCalm,
                    "peer exceeded the HTTP/2 control-frame budget",
                ));
                Err(error)
            }
        }
    }

    pub(crate) fn has_inbound(&self, stream_id: u32) -> bool {
        self.endpoint
            .streams
            .get(&stream_id)
            .is_some_and(|stream| stream.inbound.is_some())
    }

    #[cfg(test)]
    pub(crate) fn has_outbound(&self, stream_id: u32) -> bool {
        self.endpoint
            .streams
            .get(&stream_id)
            .is_some_and(|stream| stream.outbound.is_some())
    }

    pub(crate) fn knows_stream(&self, stream_id: u32) -> bool {
        self.endpoint.knows_stream(stream_id)
    }

    pub(crate) fn is_reset_tolerant(&self, stream_id: u32) -> bool {
        self.endpoint.is_reset_tolerant(stream_id)
    }

    pub(crate) fn inbound_mut(&mut self, stream_id: u32) -> Option<&mut H2StreamState> {
        self.endpoint
            .streams
            .get_mut(&stream_id)
            .and_then(|stream| stream.inbound.as_mut())
    }

    pub(crate) fn outbound(&self, stream_id: u32) -> Option<&H2OutboundHalf> {
        self.endpoint
            .streams
            .get(&stream_id)
            .and_then(|stream| stream.outbound.as_ref())
    }

    pub(crate) fn outbound_mut(&mut self, stream_id: u32) -> Option<&mut H2OutboundHalf> {
        self.endpoint
            .streams
            .get_mut(&stream_id)
            .and_then(|stream| stream.outbound.as_mut())
    }

    pub(crate) fn insert_server_stream(&mut self, stream_id: u32, inbound: Option<H2StreamState>) {
        self.endpoint.streams.insert(
            stream_id,
            H2ServerStream {
                inbound,
                outbound: Some(H2OutboundHalf::new(
                    self.endpoint.settings.initial_window_size,
                    Some(self.endpoint.http_limits.max_body_bytes()),
                )),
            },
        );
    }

    pub(crate) fn close_inbound(&mut self, stream_id: u32) {
        if let Some(stream) = self.endpoint.streams.get_mut(&stream_id) {
            stream.inbound = None;
        }
        self.retire_if_complete(stream_id, H2StreamTombstone::Closed);
    }

    pub(crate) fn close_outbound(&mut self, stream_id: u32) {
        if let Some(stream) = self.endpoint.streams.get_mut(&stream_id) {
            stream.outbound = None;
        }
        self.retire_if_complete(stream_id, H2StreamTombstone::Closed);
    }

    pub(crate) fn forget_stream(&mut self, stream_id: u32, kind: H2StreamTombstone) {
        self.endpoint.forget_stream(stream_id, kind);
    }

    pub(crate) fn retire_if_complete(&mut self, stream_id: u32, kind: H2StreamTombstone) {
        self.endpoint.retire_if_complete(stream_id, kind, |stream| {
            stream.inbound.is_none() && stream.outbound.is_none()
        });
    }

    #[cfg(test)]
    pub(crate) fn assert_stream_invariants(&self) {
        for (stream_id, stream) in &self.endpoint.streams {
            assert!(
                stream.inbound.is_some() || stream.outbound.is_some(),
                "active stream {stream_id} has no live half"
            );
        }
        self.endpoint.assert_stream_bookkeeping();
        assert_eq!(self.active_stream_count(), self.endpoint.streams.len());
    }

    pub(crate) fn apply_send_window_update(
        &mut self,
        stream_id: u32,
        increment: u32,
    ) -> Result<(), ServerError> {
        match self
            .endpoint
            .apply_send_window_update(stream_id, increment, |stream| {
                stream.outbound.as_mut().map(|half| &mut half.window)
            })? {
            H2SendWindowUpdateDisposition::Applied
            | H2SendWindowUpdateDisposition::IgnoredKnown => Ok(()),
            H2SendWindowUpdateDisposition::Unknown => {
                self.endpoint.last_protocol_error = Some(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "WINDOW_UPDATE referenced an idle client stream",
                ));
                Err(ServerError::InvalidFrame)
            }
        }
    }

    pub fn mark_local_settings_sent(&mut self) {
        self.endpoint.mark_local_settings_sent();
    }

    pub const fn local_settings_state(&self) -> H2SettingsSyncState {
        self.endpoint.local_settings_state()
    }

    pub const fn control_diagnostics(&self) -> H2ControlDiagnostics {
        self.endpoint.control_diagnostics()
    }

    pub const fn timer_obligations(&self) -> H2TimerObligations {
        H2TimerObligations {
            settings_ack: self.endpoint.local_settings_ack_debt.timer_owed(),
            graceful_shutdown_ping: matches!(
                self.endpoint.outbound_shutdown,
                H2OutboundShutdown::GracefulPingPending
            ),
        }
    }

    pub const fn timer_intent(&self) -> Option<H2TimerIntent> {
        self.timer_obligations().primary_intent()
    }

    pub fn shutdown_intent(&self) -> H2ShutdownIntent {
        match self.endpoint.outbound_shutdown {
            H2OutboundShutdown::Open => H2ShutdownIntent::None,
            H2OutboundShutdown::GracefulPingPending => H2ShutdownIntent::Drain {
                last_stream_id: H2_GRACEFUL_SHUTDOWN_LAST_STREAM_ID,
            },
            H2OutboundShutdown::GoawaySent { last_stream_id }
                if !self.endpoint.streams.is_empty() =>
            {
                H2ShutdownIntent::Drain { last_stream_id }
            }
            H2OutboundShutdown::GoawaySent { .. } => H2ShutdownIntent::Close,
        }
    }

    /// Returns the largest client-initiated stream identifier processed so far.
    pub const fn highest_processed_stream_id(&self) -> u32 {
        self.max_processed_peer_stream_id
    }

    /// Begins RFC 9113 section 6.8's two-stage graceful server shutdown.
    ///
    /// The returned bytes contain the first `NO_ERROR` GOAWAY followed by the
    /// PING whose acknowledgement advances the state machine to the final
    /// GOAWAY. An empty vector means graceful shutdown was already started.
    pub fn begin_graceful_shutdown(&mut self) -> Result<Vec<u8>, ServerError> {
        match self.endpoint.outbound_shutdown {
            H2OutboundShutdown::Open => {}
            H2OutboundShutdown::GracefulPingPending | H2OutboundShutdown::GoawaySent { .. } => {
                return Ok(Vec::new());
            }
        }

        // RFC 9113 section 6.8 uses the maximum stream ID first so a request
        // created while GOAWAY is in flight is not silently excluded.
        let mut output = Vec::new();
        encode_h2_goaway(
            &mut output,
            H2_GRACEFUL_SHUTDOWN_LAST_STREAM_ID,
            H2ErrorCode::NoError.as_u32(),
        );
        H2Frame {
            frame_type: H2FrameType::Ping,
            flags: 0,
            stream_id: 0,
            payload: H2_GRACEFUL_SHUTDOWN_PING_PAYLOAD.to_vec(),
        }
        .encode(&mut output);
        self.endpoint.control_diagnostics.goaways =
            self.endpoint.control_diagnostics.goaways.saturating_add(1);
        self.endpoint.outbound_shutdown = H2OutboundShutdown::GracefulPingPending;
        Ok(output)
    }

    /// Advances graceful shutdown when the adapter's PING wait has elapsed.
    ///
    /// This method reads no clock. A real-time or virtual-clock adapter calls
    /// it when its own deadline fires, making the transition deterministic.
    pub fn graceful_shutdown_ping_elapsed(&mut self) -> Result<Vec<u8>, ServerError> {
        let mut output = Vec::new();
        self.finish_graceful_shutdown(&mut output)?;
        Ok(output)
    }

    /// Emits connection GOAWAY with `SETTINGS_TIMEOUT` when ACK debt remains.
    ///
    /// This method reads no clock. An empty vector means no SETTINGS ACK is
    /// outstanding.
    pub fn settings_ack_timeout_elapsed(&mut self) -> Result<Vec<u8>, ServerError> {
        if !self.endpoint.local_settings_ack_debt.timer_owed() {
            return Ok(Vec::new());
        }
        self.endpoint.local_settings_ack_debt.note_timeout();
        self.goaway_frame_with_code(
            self.highest_processed_stream_id(),
            H2ErrorCode::SettingsTimeout,
        )
    }

    pub(crate) fn finish_graceful_shutdown(
        &mut self,
        output: &mut Vec<u8>,
    ) -> Result<bool, ServerError> {
        if !matches!(
            self.endpoint.outbound_shutdown,
            H2OutboundShutdown::GracefulPingPending
        ) {
            return Ok(false);
        }
        let final_goaway =
            self.goaway_frame_with_code(self.highest_processed_stream_id(), H2ErrorCode::NoError)?;
        output.extend_from_slice(&final_goaway);
        Ok(true)
    }

    pub fn goaway_frame(
        &mut self,
        last_stream_id: u32,
        error_code: u32,
    ) -> Result<Vec<u8>, ServerError> {
        self.endpoint.goaway_frame(last_stream_id, error_code)
    }

    /// Builds a GOAWAY frame with a typed HTTP/2 error code.
    pub fn goaway_frame_with_code(
        &mut self,
        last_stream_id: u32,
        error_code: H2ErrorCode,
    ) -> Result<Vec<u8>, ServerError> {
        self.goaway_frame(last_stream_id, error_code.as_u32())
    }

    /// Builds a RST_STREAM frame for a failed stream.
    pub fn rst_stream_frame(
        &mut self,
        stream_id: u32,
        error_code: u32,
    ) -> Result<Vec<u8>, ServerError> {
        if stream_id == 0 || stream_id > 0x7fff_ffff {
            return Err(ServerError::InvalidFrame);
        }
        self.endpoint.control_diagnostics.resets =
            self.endpoint.control_diagnostics.resets.saturating_add(1);
        let mut output = Vec::new();
        H2Frame {
            frame_type: H2FrameType::RstStream,
            flags: 0,
            stream_id,
            payload: error_code.to_be_bytes().to_vec(),
        }
        .encode(&mut output);
        Ok(output)
    }

    /// Builds a RST_STREAM frame with a typed HTTP/2 error code.
    pub fn rst_stream_frame_with_code(
        &mut self,
        stream_id: u32,
        error_code: H2ErrorCode,
    ) -> Result<Vec<u8>, ServerError> {
        self.rst_stream_frame(stream_id, error_code.as_u32())
    }

    pub(crate) fn take_reported_protocol_error(&mut self) -> Option<H2ProtocolError> {
        self.reported_protocol_error.take()
    }

    /// Rejects a PUSH_PROMISE frame sent by a client.
    ///
    /// Only servers may promise streams, so RFC 9113 section 8.4 requires a
    /// server that receives PUSH_PROMISE to treat it as a connection error of
    /// type PROTOCOL_ERROR.
    pub(crate) fn reject_client_push_promise(&mut self) -> ServerError {
        let error = H2ProtocolError::connection(
            H2ErrorCode::ProtocolError,
            "clients must not send PUSH_PROMISE frames",
        );
        self.endpoint.last_protocol_error = Some(error);
        self.endpoint.terminal_protocol_error = Some(error);
        ServerError::InvalidFrame
    }

    pub(crate) fn validate_peer_reset_frame(
        &mut self,
        frame: H2FrameRef<'_>,
    ) -> Result<(), ServerError> {
        if frame.stream_id == 0 {
            self.endpoint.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "RST_STREAM used stream zero",
            ));
            return Err(ServerError::InvalidFrame);
        }
        if frame.payload.len() != 4 {
            self.endpoint.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::FrameSizeError,
                "RST_STREAM payload length is not four octets",
            ));
            return Err(ServerError::InvalidFrame);
        }
        if frame.stream_id.is_multiple_of(2) || !self.knows_stream(frame.stream_id) {
            self.endpoint.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "RST_STREAM referenced an idle client stream",
            ));
            return Err(ServerError::InvalidFrame);
        }
        Ok(())
    }

    pub(crate) fn validate_peer_priority_frame(
        &mut self,
        frame: H2FrameRef<'_>,
    ) -> Result<(), ServerError> {
        if let Err(error) = priority_payload(frame.stream_id, frame.payload) {
            self.endpoint.last_protocol_error = Some(if frame.stream_id == 0 {
                H2ProtocolError::connection(H2ErrorCode::ProtocolError, "PRIORITY used stream zero")
            } else if frame.payload.len() != 5 {
                H2ProtocolError::connection(
                    H2ErrorCode::FrameSizeError,
                    "PRIORITY payload length is not five octets",
                )
            } else {
                H2ProtocolError::stream(
                    frame.stream_id,
                    H2ErrorCode::ProtocolError,
                    "PRIORITY dependency references its own stream",
                )
            });
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn peer_window_update_increment(
        &mut self,
        frame: H2FrameRef<'_>,
    ) -> Result<u32, ServerError> {
        match window_update_increment(frame.payload) {
            Ok(increment) => Ok(increment),
            Err(error) => {
                self.endpoint.last_protocol_error = Some(if frame.payload.len() != 4 {
                    H2ProtocolError::connection(
                        H2ErrorCode::FrameSizeError,
                        "WINDOW_UPDATE payload length is not four octets",
                    )
                } else if frame.stream_id == 0 {
                    H2ProtocolError::connection(
                        H2ErrorCode::ProtocolError,
                        "connection WINDOW_UPDATE increment is zero",
                    )
                } else {
                    H2ProtocolError::stream(
                        frame.stream_id,
                        H2ErrorCode::ProtocolError,
                        "stream WINDOW_UPDATE increment is zero",
                    )
                });
                Err(error)
            }
        }
    }

    pub(crate) fn peer_data_payload<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<&'a [u8], ServerError> {
        match data_payload(frame.flags, frame.payload) {
            Ok(payload) => Ok(payload),
            Err(error) => {
                self.endpoint.last_protocol_error = Some(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "DATA padding is invalid",
                ));
                Err(error)
            }
        }
    }

    /// Accepts bytes through the compatibility request-only interface.
    ///
    /// Reset-tolerant DATA is consumed without delivering a request, preserving
    /// the behavior from before discarded-DATA events were exposed. Owners that
    /// account connection receive credit must use [`Self::accept_event_ref`] or
    /// [`Self::accept_event`] instead.
    pub fn accept(
        &mut self,
        input: &[u8],
    ) -> Result<(Option<H2Request>, usize, Vec<u8>), ServerError> {
        let mut consumed = 0usize;
        let mut output = Vec::new();
        while consumed < input.len() {
            let (event, used, mut event_output) = self.accept_event_ref(&input[consumed..])?;
            consumed += used;
            output.append(&mut event_output);
            match event {
                Some(H2StreamEvent::RequestHeaders { request, .. }) => {
                    return Ok((Some(request), consumed, output));
                }
                Some(
                    H2StreamEvent::Settings { .. }
                    | H2StreamEvent::Ping { .. }
                    | H2StreamEvent::WindowUpdate { .. }
                    | H2StreamEvent::DiscardedData { .. },
                ) => {}
                Some(_) => return Err(ServerError::InvalidFrame),
                None => return Ok((None, consumed, output)),
            }
            if used == 0 {
                return Ok((None, consumed, output));
            }
        }
        Ok((None, consumed, output))
    }

    pub fn accept_event_bytes(
        &mut self,
        input: &[u8],
    ) -> Result<(Option<H2ByteStreamEvent>, usize, Vec<u8>), ServerError> {
        let (event, consumed, output) = self.accept_event_bytes_ref(input)?;
        if let Some(error) = self.handled_compat_error.take() {
            self.reported_protocol_error = self.handled_stream_error;
            return Err(error);
        }
        Ok((
            event.map(H2ByteStreamEventRef::into_owned),
            consumed,
            output,
        ))
    }

    pub fn accept_event_bytes_ref<'a>(
        &mut self,
        input: &'a [u8],
    ) -> Result<(Option<H2ByteStreamEventRef<'a>>, usize, Vec<u8>), ServerError> {
        self.reported_protocol_error = None;
        if let Some(error) = self.endpoint.terminal_protocol_error {
            self.reported_protocol_error = Some(error);
            return Err(error.into());
        }
        let mut consumed = 0usize;
        let mut output = Vec::new();
        if !self.preface_seen {
            if input.len() < CLIENT_PREFACE.len() {
                if CLIENT_PREFACE.starts_with(input) {
                    return Ok((None, 0, output));
                }
                self.reported_protocol_error = Some(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "invalid HTTP/2 client preface",
                ));
                return Err(ServerError::InvalidPreface);
            }
            if &input[..CLIENT_PREFACE.len()] != CLIENT_PREFACE {
                self.reported_protocol_error = Some(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "invalid HTTP/2 client preface",
                ));
                return Err(ServerError::InvalidPreface);
            }
            self.preface_seen = true;
            consumed += CLIENT_PREFACE.len();
        }
        if !self.endpoint.settings_seen {
            let (frame, used) = match H2FrameRef::decode_outcome_with_max_frame_size(
                &input[consumed..],
                self.endpoint.limits.max_frame_size,
            ) {
                H2FrameRefDecodeOutcome::Frame { frame, consumed } => (frame, consumed),
                H2FrameRefDecodeOutcome::NeedMore => return Ok((None, consumed, output)),
                H2FrameRefDecodeOutcome::Error(error) => {
                    self.reported_protocol_error = Some(error);
                    return Err(error.into());
                }
            };
            if frame.frame_type != H2FrameType::Settings
                || frame.flags & 0x1 != 0
                || frame.stream_id != 0
            {
                self.reported_protocol_error = Some(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "HTTP/2 client connection preface must start with SETTINGS",
                ));
                return Err(ServerError::InvalidFrame);
            }
            if let Err(error) = self.apply_settings(frame.payload) {
                let protocol_error =
                    self.h2_error_from_server_error(error.clone(), Some(frame.head()));
                self.reported_protocol_error = Some(protocol_error);
                return Err(error);
            }
            self.endpoint.settings_seen = true;
            consumed += used;
            self.encode_local_settings(&mut output);
            H2Frame {
                frame_type: H2FrameType::Settings,
                flags: 0x1,
                stream_id: 0,
                payload: Vec::new(),
            }
            .encode(&mut output);
            self.encode_local_connection_window_update(&mut output);
        }
        if input.len() == consumed {
            return Ok((None, consumed, output));
        }
        let (frame, used) = match H2FrameRef::decode_outcome_with_max_frame_size(
            &input[consumed..],
            self.endpoint.limits.max_frame_size,
        ) {
            H2FrameRefDecodeOutcome::Frame { frame, consumed } => (frame, consumed),
            H2FrameRefDecodeOutcome::NeedMore => return Ok((None, consumed, output)),
            H2FrameRefDecodeOutcome::Error(error) => {
                self.reported_protocol_error = Some(error);
                return Err(error.into());
            }
        };
        consumed += used;
        let (event, mut event_output) = self.accept_frame_bytes_ref(frame)?;
        output.append(&mut event_output);
        Ok((event, consumed, output))
    }

    pub(crate) fn accept_driver_event_bytes_ref<'a>(
        &mut self,
        input: &'a [u8],
    ) -> Result<H2ServerDriverProgress<'a>, ServerError> {
        self.reported_protocol_error = None;
        if let Some(error) = self.endpoint.terminal_protocol_error {
            self.reported_protocol_error = Some(error);
            return Err(error.into());
        }
        let mut consumed = 0usize;
        let mut output = Vec::new();
        if !self.preface_seen {
            if input.len() < CLIENT_PREFACE.len() {
                if CLIENT_PREFACE.starts_with(input) {
                    return Ok(H2ServerDriverProgress {
                        event: None,
                        consumed: 0,
                        output,
                        handled_protocol_error: None,
                        handled_compat_error: None,
                    });
                }
                self.reported_protocol_error = Some(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "invalid HTTP/2 client preface",
                ));
                return Err(ServerError::InvalidPreface);
            }
            if &input[..CLIENT_PREFACE.len()] != CLIENT_PREFACE {
                self.reported_protocol_error = Some(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "invalid HTTP/2 client preface",
                ));
                return Err(ServerError::InvalidPreface);
            }
            self.preface_seen = true;
            consumed += CLIENT_PREFACE.len();
        }
        if !self.endpoint.settings_seen {
            let (frame, used) = match H2FrameRef::decode_outcome_with_max_frame_size(
                &input[consumed..],
                self.endpoint.limits.max_frame_size,
            ) {
                H2FrameRefDecodeOutcome::Frame { frame, consumed } => (frame, consumed),
                H2FrameRefDecodeOutcome::NeedMore => {
                    return Ok(H2ServerDriverProgress {
                        event: None,
                        consumed,
                        output,
                        handled_protocol_error: None,
                        handled_compat_error: None,
                    });
                }
                H2FrameRefDecodeOutcome::Error(error) => {
                    self.reported_protocol_error = Some(error);
                    return Err(error.into());
                }
            };
            if frame.frame_type != H2FrameType::Settings
                || frame.flags & 0x1 != 0
                || frame.stream_id != 0
            {
                self.reported_protocol_error = Some(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "HTTP/2 client connection preface must start with SETTINGS",
                ));
                return Err(ServerError::InvalidFrame);
            }
            if let Err(error) = self.apply_settings(frame.payload) {
                let protocol_error =
                    self.h2_error_from_server_error(error.clone(), Some(frame.head()));
                self.reported_protocol_error = Some(protocol_error);
                return Err(error);
            }
            self.endpoint.settings_seen = true;
            consumed += used;
            self.encode_local_settings(&mut output);
            H2Frame {
                frame_type: H2FrameType::Settings,
                flags: 0x1,
                stream_id: 0,
                payload: Vec::new(),
            }
            .encode(&mut output);
            self.encode_local_connection_window_update(&mut output);
        }
        if input.len() == consumed {
            return Ok(H2ServerDriverProgress {
                event: None,
                consumed,
                output,
                handled_protocol_error: None,
                handled_compat_error: None,
            });
        }
        let (frame, used) = match H2FrameRef::decode_outcome_with_max_frame_size(
            &input[consumed..],
            self.endpoint.limits.max_frame_size,
        ) {
            H2FrameRefDecodeOutcome::Frame { frame, consumed } => (frame, consumed),
            H2FrameRefDecodeOutcome::NeedMore => {
                return Ok(H2ServerDriverProgress {
                    event: None,
                    consumed,
                    output,
                    handled_protocol_error: None,
                    handled_compat_error: None,
                });
            }
            H2FrameRefDecodeOutcome::Error(error) => {
                self.reported_protocol_error = Some(error);
                return Err(error.into());
            }
        };
        consumed += used;
        let mut frame_progress = self.accept_driver_frame_bytes_ref(frame)?;
        output.append(&mut frame_progress.output);
        Ok(H2ServerDriverProgress {
            event: frame_progress.event,
            consumed,
            output,
            handled_protocol_error: frame_progress.handled_protocol_error,
            handled_compat_error: frame_progress.handled_compat_error,
        })
    }

    fn accept_driver_frame_bytes_ref<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<H2ServerDriverProgress<'a>, ServerError> {
        self.endpoint.last_compat_error = None;
        self.last_compat_error_recoverable = false;
        self.handled_compat_error = None;
        self.handled_stream_error = None;
        if let Some(error) = self.endpoint.terminal_protocol_error {
            self.reported_protocol_error = Some(error);
            return Err(error.into());
        }
        if !self.endpoint.settings_seen
            && (!matches!(frame.frame_type, H2FrameType::Settings)
                || frame.flags & 0x1 != 0
                || frame.stream_id != 0)
        {
            self.reported_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "HTTP/2 client connection preface must start with SETTINGS",
            ));
            return Err(ServerError::InvalidFrame);
        }
        if let Some(pending) = self.endpoint.header_block.pending.as_ref()
            && (!matches!(frame.frame_type, H2FrameType::Continuation)
                || frame.stream_id != pending.stream_id)
        {
            self.reported_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "HTTP/2 header block continuation sequence violated",
            ));
            return Err(ServerError::InvalidFrame);
        }
        let head = frame.head();
        match self.dispatch_peer_frame_driver(frame) {
            Ok((event, output)) => Ok(H2ServerDriverProgress {
                event,
                consumed: 0,
                output,
                handled_protocol_error: None,
                handled_compat_error: None,
            }),
            Err(error) => {
                let explicitly_classified = self.endpoint.last_protocol_error.is_some();
                let protocol_error = self.h2_error_from_server_error(error.clone(), Some(head));
                if let H2ProtocolError {
                    scope: H2ErrorScope::Stream(_),
                    code: H2ErrorCode::RefusedStream,
                    ..
                } = protocol_error
                {
                    let output = self.handle_stream_error(protocol_error)?;
                    return Ok(H2ServerDriverProgress {
                        event: None,
                        consumed: 0,
                        output,
                        handled_protocol_error: self.handled_stream_error.take(),
                        handled_compat_error: None,
                    });
                }
                self.last_compat_error_recoverable =
                    explicitly_classified || recoverable_h2_message_error(&error);
                self.endpoint.last_compat_error = Some(error);
                let output = self.finish_compat_frame_error(protocol_error)?;
                Ok(H2ServerDriverProgress {
                    event: None,
                    consumed: 0,
                    output,
                    handled_protocol_error: self.handled_stream_error.take(),
                    handled_compat_error: self.handled_compat_error.take(),
                })
            }
        }
    }

    fn dispatch_peer_frame_driver<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<(Option<H2DriverServerEvent<'a>>, Vec<u8>), ServerError> {
        if !matches!(
            frame.frame_type,
            H2FrameType::Headers | H2FrameType::Continuation
        ) {
            let (event, output) = self.dispatch_peer_frame(frame, H2ServerFrameOutput::Emit)?;
            return Ok((event.map(H2DriverServerEvent::from_non_header), output));
        }
        let Some(block) = accept_header_frame_for_connection(
            &mut self.endpoint.header_block,
            frame,
            self.endpoint.limits,
            &mut self.endpoint.last_protocol_error,
            &mut self.endpoint.terminal_protocol_error,
        )?
        else {
            return Ok((None, Vec::new()));
        };
        let event = self.event_from_complete_headers_driver(block)?;
        Ok((Some(event), Vec::new()))
    }

    fn event_from_complete_headers_driver<'a>(
        &mut self,
        block: H2CompleteHeaderBlock<'_>,
    ) -> Result<H2DriverServerEvent<'a>, ServerError> {
        let stream_id = block.stream_id;
        let flags = block.flags;
        let role = if self.has_inbound(stream_id) {
            H2HeaderValidationRole::Trailers
        } else {
            H2HeaderValidationRole::Request
        };
        let headers =
            self.endpoint
                .decode_compact_h2_header_fields(stream_id, &block.block, role)?;
        let disposition = self.validate_header_stream_state(stream_id, flags)?;
        if block.self_dependency {
            self.endpoint.last_protocol_error = Some(H2ProtocolError::stream(
                stream_id,
                H2ErrorCode::ProtocolError,
                "HEADERS priority dependency references its own stream",
            ));
            return Err(ServerError::InvalidFrame);
        }
        if let H2HeaderStreamDisposition::Refuse = disposition {
            self.record_refused_stream(stream_id);
            return Err(ServerError::InvalidFrame);
        }
        let event = self.event_from_compact_header_fields(stream_id, flags, headers)?;
        self.record_progress_frame();
        Ok(event)
    }

    fn event_from_compact_header_fields<'a>(
        &mut self,
        stream_id: u32,
        flags: u8,
        section: ValidatedSection,
    ) -> Result<H2DriverServerEvent<'a>, ServerError> {
        let end_stream = flags & 0x1 != 0;
        if self.has_inbound(stream_id) {
            if !end_stream {
                return Err(ServerError::InvalidFrame);
            }
            let limits = self.endpoint.http_limits;
            match self.inbound_mut(stream_id) {
                Some(state) => state
                    .accept_trailers(section, limits)
                    .and_then(|()| state.finish())?,
                None => return Err(ServerError::InvalidFrame),
            }
            self.close_inbound(stream_id);
            Ok(H2DriverServerEvent::Trailers { stream_id })
        } else {
            if self
                .endpoint
                .outbound_shutdown
                .sent_last_stream_id()
                .is_some_and(|last_stream_id| stream_id > last_stream_id)
                || stream_id.is_multiple_of(2)
                || stream_id <= self.max_peer_stream_id
                || self.knows_stream(stream_id)
                || self.active_stream_count() >= self.endpoint.limits.max_active_streams
            {
                return Err(ServerError::InvalidFrame);
            }
            self.max_peer_stream_id = stream_id;
            let inbound = if end_stream {
                H2StreamState::new_raw(
                    section,
                    self.endpoint.http_limits,
                    self.request_body_limit,
                )?
                .finish()?;
                None
            } else {
                Some(H2StreamState::new_raw(
                    section,
                    self.endpoint.http_limits,
                    self.request_body_limit,
                )?)
            };
            self.insert_server_stream(stream_id, inbound);
            self.max_processed_peer_stream_id = stream_id;
            Ok(H2DriverServerEvent::RequestHeaders {
                stream_id,
                section,
                end_stream,
            })
        }
    }

    pub fn accept_event(
        &mut self,
        input: &[u8],
    ) -> Result<(Option<H2StreamEvent>, usize, Vec<u8>), ServerError> {
        let (event, consumed, output) = self.accept_event_bytes(input)?;
        if let Some(error) = self.handled_compat_error.take() {
            self.reported_protocol_error = self.handled_stream_error;
            return Err(error);
        }
        let event = event
            .map(H2ByteStreamEvent::try_into_text)
            .transpose()
            .map_err(|_| ServerError::MalformedMessage)?;
        Ok((event, consumed, output))
    }

    pub fn accept_event_ref<'a>(
        &mut self,
        input: &'a [u8],
    ) -> Result<(Option<H2StreamEventRef<'a>>, usize, Vec<u8>), ServerError> {
        let (event, consumed, output) = self.accept_event_bytes_ref(input)?;
        if let Some(error) = self.handled_compat_error.take() {
            self.reported_protocol_error = self.handled_stream_error;
            return Err(error);
        }
        let event = event
            .map(H2ByteStreamEvent::try_into_text)
            .transpose()
            .map_err(|_| ServerError::MalformedMessage)?;
        Ok((event, consumed, output))
    }

    pub fn accept_frame_bytes(
        &mut self,
        frame: H2Frame,
    ) -> Result<(Option<H2ByteStreamEvent>, Vec<u8>), ServerError> {
        let (outcome, output) = self.accept_frame_bytes_typed(frame);
        match outcome {
            H2FrameOutcome::Event(event) => Ok((Some(event), output)),
            H2FrameOutcome::Ignored => Ok((None, output)),
            H2FrameOutcome::Error(error) => {
                let output = self.finish_compat_frame_error(error)?;
                Ok((None, output))
            }
        }
    }

    pub fn accept_frame_bytes_ref<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<(Option<H2ByteStreamEventRef<'a>>, Vec<u8>), ServerError> {
        let (outcome, output) = self.accept_frame_bytes_ref_typed(frame);
        match outcome {
            H2FrameOutcome::Event(event) => Ok((Some(event), output)),
            H2FrameOutcome::Ignored => Ok((None, output)),
            H2FrameOutcome::Error(error) => {
                let output = self.finish_compat_frame_error(error)?;
                Ok((None, output))
            }
        }
    }

    pub fn accept_frame_bytes_typed(
        &mut self,
        frame: H2Frame,
    ) -> (H2FrameOutcome<H2ByteStreamEvent>, Vec<u8>) {
        let (outcome, output) = self.accept_frame_bytes_ref_typed(frame.as_ref());
        (outcome.map_event(H2ByteStreamEventRef::into_owned), output)
    }

    pub fn accept_frame_bytes_ref_typed<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> (H2FrameOutcome<H2ByteStreamEventRef<'a>>, Vec<u8>) {
        self.endpoint.last_compat_error = None;
        self.last_compat_error_recoverable = false;
        self.handled_compat_error = None;
        self.handled_stream_error = None;
        if let Some(error) = self.endpoint.terminal_protocol_error {
            return (H2FrameOutcome::Error(error), Vec::new());
        }
        if !self.endpoint.settings_seen
            && (!matches!(frame.frame_type, H2FrameType::Settings)
                || frame.flags & 0x1 != 0
                || frame.stream_id != 0)
        {
            return (
                H2FrameOutcome::Error(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "HTTP/2 client connection preface must start with SETTINGS",
                )),
                Vec::new(),
            );
        }
        if let Some(pending) = self.endpoint.header_block.pending.as_ref()
            && (!matches!(frame.frame_type, H2FrameType::Continuation)
                || frame.stream_id != pending.stream_id)
        {
            return (
                H2FrameOutcome::Error(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "HTTP/2 header block continuation sequence violated",
                )),
                Vec::new(),
            );
        }
        let head = frame.head();
        match self.accept_frame_compat(frame) {
            Ok((Some(event), output)) => (H2FrameOutcome::Event(event), output),
            Ok((None, output)) => (H2FrameOutcome::Ignored, output),
            Err(error) => {
                let explicitly_classified = self.endpoint.last_protocol_error.is_some();
                let typed = self.h2_error_from_server_error(error.clone(), Some(head));
                if let H2ProtocolError {
                    scope: H2ErrorScope::Stream(_),
                    code: H2ErrorCode::RefusedStream,
                    ..
                } = typed
                {
                    return match self.handle_stream_error(typed) {
                        Ok(output) => (H2FrameOutcome::Ignored, output),
                        Err(reset_error) => {
                            self.endpoint.last_compat_error = Some(reset_error.clone());
                            (
                                H2FrameOutcome::Error(
                                    self.h2_error_from_server_error(reset_error, Some(head)),
                                ),
                                Vec::new(),
                            )
                        }
                    };
                }
                self.last_compat_error_recoverable =
                    explicitly_classified || recoverable_h2_message_error(&error);
                self.endpoint.last_compat_error = Some(error);
                (H2FrameOutcome::Error(typed), Vec::new())
            }
        }
    }

    pub fn accept_frame(
        &mut self,
        frame: H2Frame,
    ) -> Result<(Option<H2StreamEvent>, Vec<u8>), ServerError> {
        let (outcome, output) = self.accept_frame_typed(frame);
        match outcome {
            H2FrameOutcome::Event(event) => Ok((Some(event), output)),
            H2FrameOutcome::Ignored => Ok((None, output)),
            H2FrameOutcome::Error(error) => Err(error.into()),
        }
    }

    pub fn accept_frame_ref<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<(Option<H2StreamEventRef<'a>>, Vec<u8>), ServerError> {
        let (outcome, output) = self.accept_frame_ref_typed(frame);
        match outcome {
            H2FrameOutcome::Event(event) => Ok((Some(event), output)),
            H2FrameOutcome::Ignored => Ok((None, output)),
            H2FrameOutcome::Error(error) => Err(error.into()),
        }
    }

    pub fn accept_frame_typed(
        &mut self,
        frame: H2Frame,
    ) -> (H2FrameOutcome<H2StreamEvent>, Vec<u8>) {
        let stream_id = frame.stream_id;
        let (outcome, output) = self.accept_frame_bytes_typed(frame);
        (project_stream_outcome(outcome, stream_id), output)
    }

    pub fn accept_frame_ref_typed<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> (H2FrameOutcome<H2StreamEventRef<'a>>, Vec<u8>) {
        let stream_id = frame.stream_id;
        let (outcome, output) = self.accept_frame_bytes_ref_typed(frame);
        (project_stream_outcome(outcome, stream_id), output)
    }

    pub(crate) fn accept_frame_compat<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<(Option<H2ByteStreamEventRef<'a>>, Vec<u8>), ServerError> {
        self.dispatch_peer_frame(frame, H2ServerFrameOutput::Emit)
    }

    pub(crate) fn dispatch_peer_frame<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
        output_policy: H2ServerFrameOutput,
    ) -> Result<(Option<H2ByteStreamEventRef<'a>>, Vec<u8>), ServerError> {
        if self.endpoint.header_block.pending.is_some()
            && !matches!(frame.frame_type, H2FrameType::Continuation)
        {
            return Err(ServerError::InvalidFrame);
        }
        let emit = matches!(output_policy, H2ServerFrameOutput::Emit);
        let mut output = Vec::new();
        let event = match frame.frame_type {
            H2FrameType::Settings => {
                self.record_control(H2FrameType::Settings)?;
                if frame.stream_id != 0 {
                    return Err(ServerError::InvalidFrame);
                }
                let initial_window_size = if frame.flags & 0x1 != 0 {
                    if !frame.payload.is_empty() {
                        return Err(ServerError::InvalidFrame);
                    }
                    self.endpoint.local_settings_ack_debt.note_ack()?;
                    self.endpoint.control_diagnostics.settings_acks = self
                        .endpoint
                        .control_diagnostics
                        .settings_acks
                        .saturating_add(1);
                    None
                } else {
                    self.endpoint.settings_seen = true;
                    self.endpoint.control_diagnostics.settings_frames = self
                        .endpoint
                        .control_diagnostics
                        .settings_frames
                        .saturating_add(1);
                    let initial_window_size = self.apply_settings(frame.payload)?;
                    if emit {
                        H2Frame {
                            frame_type: H2FrameType::Settings,
                            flags: 0x1,
                            stream_id: 0,
                            payload: Vec::new(),
                        }
                        .encode(&mut output);
                    }
                    initial_window_size
                };
                H2ByteStreamEvent::Settings {
                    initial_window_size,
                }
            }
            H2FrameType::WindowUpdate => {
                self.record_control(H2FrameType::WindowUpdate)?;
                let increment = self.peer_window_update_increment(frame)?;
                self.apply_send_window_update(frame.stream_id, increment)?;
                H2ByteStreamEvent::WindowUpdate {
                    stream_id: frame.stream_id,
                    increment,
                }
            }
            H2FrameType::Ping => {
                self.record_control(H2FrameType::Ping)?;
                if frame.stream_id != 0 || frame.payload.len() != 8 {
                    return Err(ServerError::InvalidFrame);
                }
                let ack = frame.flags & 0x1 != 0;
                if ack {
                    self.endpoint.control_diagnostics.ping_acks = self
                        .endpoint
                        .control_diagnostics
                        .ping_acks
                        .saturating_add(1);
                } else {
                    self.endpoint.control_diagnostics.pings =
                        self.endpoint.control_diagnostics.pings.saturating_add(1);
                }
                if emit {
                    if !ack {
                        H2FrameRef {
                            frame_type: H2FrameType::Ping,
                            flags: 0x1,
                            stream_id: 0,
                            payload: frame.payload,
                        }
                        .encode(&mut output);
                    } else if frame.payload == H2_GRACEFUL_SHUTDOWN_PING_PAYLOAD {
                        self.finish_graceful_shutdown(&mut output)?;
                    }
                }
                H2ByteStreamEvent::Ping { ack }
            }
            H2FrameType::Priority => {
                self.record_control(H2FrameType::Priority)?;
                self.validate_peer_priority_frame(frame)?;
                return Ok((None, output));
            }
            H2FrameType::RstStream => {
                self.record_control(H2FrameType::RstStream)?;
                self.validate_peer_reset_frame(frame)?;
                self.endpoint.control_diagnostics.resets =
                    self.endpoint.control_diagnostics.resets.saturating_add(1);
                // The peer knows it reset this stream, so nothing it sends
                // afterwards can be in flight and reset tolerance would only
                // mask a protocol violation.
                self.forget_stream(frame.stream_id, H2StreamTombstone::Closed);
                H2ByteStreamEvent::Reset {
                    stream_id: frame.stream_id,
                    error_code: u32::from_be_bytes([
                        frame.payload[0],
                        frame.payload[1],
                        frame.payload[2],
                        frame.payload[3],
                    ]),
                }
            }
            H2FrameType::Headers => {
                if let Some(block) = accept_header_frame_for_connection(
                    &mut self.endpoint.header_block,
                    frame,
                    self.endpoint.limits,
                    &mut self.endpoint.last_protocol_error,
                    &mut self.endpoint.terminal_protocol_error,
                )? {
                    self.event_from_complete_headers(block)?
                } else {
                    return Ok((None, output));
                }
            }
            H2FrameType::Data => {
                let payload = self.peer_data_payload(frame)?;
                let discard = self.is_reset_tolerant(frame.stream_id);
                self.validate_data_frame(frame.stream_id, payload.len(), frame.flags & 0x1 != 0)?;
                if !payload.is_empty() || frame.flags & 0x1 != 0 {
                    self.record_progress_frame();
                }
                if discard {
                    H2ByteStreamEvent::DiscardedData {
                        stream_id: frame.stream_id,
                        flow_control_len: frame.payload.len(),
                    }
                } else {
                    H2ByteStreamEvent::Data {
                        stream_id: frame.stream_id,
                        payload,
                        flow_control_len: frame.payload.len(),
                        end_stream: frame.flags & 0x1 != 0,
                    }
                }
            }
            H2FrameType::Goaway => {
                self.record_control(H2FrameType::Goaway)?;
                if frame.stream_id != 0 || frame.payload.len() < 8 {
                    return Err(ServerError::InvalidFrame);
                }
                let mut last_stream_id = u32::from_be_bytes([
                    frame.payload[0],
                    frame.payload[1],
                    frame.payload[2],
                    frame.payload[3],
                ]);
                last_stream_id &= 0x7fff_ffff;
                self.endpoint.received_goaway_last_stream_id = Some(last_stream_id);
                self.endpoint.control_diagnostics.goaways =
                    self.endpoint.control_diagnostics.goaways.saturating_add(1);
                H2ByteStreamEvent::Goaway {
                    last_stream_id,
                    error_code: u32::from_be_bytes([
                        frame.payload[4],
                        frame.payload[5],
                        frame.payload[6],
                        frame.payload[7],
                    ]),
                }
            }
            H2FrameType::Continuation => {
                if let Some(block) = accept_header_frame_for_connection(
                    &mut self.endpoint.header_block,
                    frame,
                    self.endpoint.limits,
                    &mut self.endpoint.last_protocol_error,
                    &mut self.endpoint.terminal_protocol_error,
                )? {
                    self.event_from_complete_headers(block)?
                } else {
                    return Ok((None, output));
                }
            }
            H2FrameType::PushPromise => {
                return Err(self.reject_client_push_promise());
            }
            H2FrameType::Unknown(_) => return Ok((None, output)),
        };
        Ok((Some(event), output))
    }

    /// Classifies a frame while advancing HTTP/2 connection state.
    ///
    /// This is not a pure inspection helper: SETTINGS, HPACK/header-block,
    /// stream lifecycle, control-budget, and diagnostic state are updated.
    pub fn classify_frame(&mut self, frame: H2Frame) -> Result<Option<H2StreamEvent>, ServerError> {
        match self.classify_frame_typed(frame) {
            H2FrameOutcome::Event(event) => Ok(Some(event)),
            H2FrameOutcome::Ignored => Ok(None),
            H2FrameOutcome::Error(error) => Err(error.into()),
        }
    }

    /// Borrowed variant of [`Self::classify_frame`].
    pub fn classify_frame_ref<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<Option<H2StreamEventRef<'a>>, ServerError> {
        match self.classify_frame_ref_typed(frame) {
            H2FrameOutcome::Event(event) => Ok(Some(event)),
            H2FrameOutcome::Ignored => Ok(None),
            H2FrameOutcome::Error(error) => Err(error.into()),
        }
    }

    /// Typed variant of [`Self::classify_frame`] with the same stateful effects.
    pub fn classify_frame_typed(&mut self, frame: H2Frame) -> H2FrameOutcome<H2StreamEvent> {
        let stream_id = frame.stream_id;
        project_stream_outcome(
            self.classify_frame_bytes_ref_typed(frame.as_ref())
                .map_event(H2ByteStreamEventRef::into_owned),
            stream_id,
        )
    }

    /// Borrowed typed variant of [`Self::classify_frame`].
    pub fn classify_frame_ref_typed<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> H2FrameOutcome<H2StreamEventRef<'a>> {
        let stream_id = frame.stream_id;
        project_stream_outcome(self.classify_frame_bytes_ref_typed(frame), stream_id)
    }

    pub(crate) fn classify_frame_bytes_ref_typed<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> H2FrameOutcome<H2ByteStreamEventRef<'a>> {
        if let Some(error) = self.endpoint.terminal_protocol_error {
            return H2FrameOutcome::Error(error);
        }
        if !self.endpoint.settings_seen
            && (!matches!(frame.frame_type, H2FrameType::Settings)
                || frame.flags & 0x1 != 0
                || frame.stream_id != 0)
        {
            return H2FrameOutcome::Error(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "HTTP/2 client connection preface must start with SETTINGS",
            ));
        }
        if let Some(pending) = self.endpoint.header_block.pending.as_ref()
            && (!matches!(frame.frame_type, H2FrameType::Continuation)
                || frame.stream_id != pending.stream_id)
        {
            return H2FrameOutcome::Error(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "HTTP/2 header block continuation sequence violated",
            ));
        }
        let head = frame.head();
        match self.accept_frame_without_output(frame) {
            Ok((Some(event), ())) => H2FrameOutcome::Event(event),
            Ok((None, ())) => H2FrameOutcome::Ignored,
            Err(error) => H2FrameOutcome::Error(self.h2_error_from_server_error(error, Some(head))),
        }
    }

    pub(crate) fn accept_frame_without_output<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<(Option<H2ByteStreamEventRef<'a>>, ()), ServerError> {
        self.dispatch_peer_frame(frame, H2ServerFrameOutput::Suppress)
            .map(|(event, _)| (event, ()))
    }

    pub(crate) fn event_from_complete_headers<'a>(
        &mut self,
        block: H2CompleteHeaderBlock<'a>,
    ) -> Result<H2ByteStreamEventRef<'a>, ServerError> {
        let event = self.event_from_complete_headers_parts::<&'a [u8]>(
            block.stream_id,
            block.flags,
            &block.block,
            block.self_dependency,
        )?;
        self.record_progress_frame();
        Ok(event)
    }

    pub(crate) fn event_from_complete_headers_parts<P>(
        &mut self,
        stream_id: u32,
        flags: u8,
        block: &[u8],
        self_dependency: bool,
    ) -> Result<H2ByteStreamEvent<P>, ServerError> {
        let role = if self.has_inbound(stream_id) {
            H2HeaderValidationRole::Trailers
        } else {
            H2HeaderValidationRole::Request
        };
        let (headers, section) = self.decode_h2_header_fields(stream_id, block, role)?;
        let disposition = match self.validate_header_stream_state(stream_id, flags) {
            Ok(disposition) => disposition,
            Err(error) => {
                self.endpoint.recycle_decoded_header_fields(headers);
                return Err(error);
            }
        };
        if self_dependency {
            self.endpoint.last_protocol_error = Some(H2ProtocolError::stream(
                stream_id,
                H2ErrorCode::ProtocolError,
                "HEADERS priority dependency references its own stream",
            ));
            self.endpoint.recycle_decoded_header_fields(headers);
            return Err(ServerError::InvalidFrame);
        }
        // A refused block can mutate the connection-wide HPACK table, so the
        // capacity decision must happen only after the complete block decodes.
        if let H2HeaderStreamDisposition::Refuse = disposition {
            self.record_refused_stream(stream_id);
            self.endpoint.recycle_decoded_header_fields(headers);
            return Err(ServerError::InvalidFrame);
        }
        self.event_from_decoded_header_fields(stream_id, flags, headers, section)
    }

    pub(crate) fn validate_header_stream_state(
        &mut self,
        stream_id: u32,
        flags: u8,
    ) -> Result<H2HeaderStreamDisposition, ServerError> {
        if self.has_inbound(stream_id) {
            return if flags & 0x1 != 0 {
                Ok(H2HeaderStreamDisposition::Accept)
            } else {
                self.endpoint.last_protocol_error = Some(H2ProtocolError::stream(
                    stream_id,
                    H2ErrorCode::ProtocolError,
                    "request trailers did not end the stream",
                ));
                Err(ServerError::InvalidFrame)
            };
        }
        if stream_id == 0 || stream_id.is_multiple_of(2) || stream_id > 0x7fff_ffff {
            self.endpoint.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "client used an invalid request stream identifier",
            ));
            return Err(ServerError::InvalidFrame);
        }
        if self
            .endpoint
            .outbound_shutdown
            .sent_last_stream_id()
            .is_some_and(|last_stream_id| stream_id > last_stream_id)
        {
            self.endpoint.last_protocol_error = Some(H2ProtocolError::stream(
                stream_id,
                H2ErrorCode::RefusedStream,
                "new stream arrived after GOAWAY",
            ));
            return Err(ServerError::InvalidFrame);
        }
        if self.knows_stream(stream_id) {
            self.endpoint.last_protocol_error = Some(H2ProtocolError::stream(
                stream_id,
                H2ErrorCode::StreamClosed,
                "HEADERS arrived after the stream closed",
            ));
            return Err(ServerError::InvalidFrame);
        }
        if stream_id <= self.max_peer_stream_id {
            self.endpoint.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "client opened a stream identifier out of order",
            ));
            return Err(ServerError::InvalidFrame);
        }
        if self.active_stream_count() >= self.endpoint.limits.max_active_streams {
            Ok(H2HeaderStreamDisposition::Refuse)
        } else {
            Ok(H2HeaderStreamDisposition::Accept)
        }
    }

    pub(crate) fn record_refused_stream(&mut self, stream_id: u32) {
        // RFC 9113 section 5.1.2 makes this a stream error so siblings survive.
        self.max_peer_stream_id = stream_id;
        self.endpoint
            .remember_tombstone(stream_id, H2StreamTombstone::ResetTolerant);
        self.endpoint.last_protocol_error = Some(H2ProtocolError::stream(
            stream_id,
            H2ErrorCode::RefusedStream,
            "peer exceeded SETTINGS_MAX_CONCURRENT_STREAMS",
        ));
    }

    pub(crate) fn event_from_decoded_header_fields<P>(
        &mut self,
        stream_id: u32,
        flags: u8,
        headers: Vec<H2HeaderField>,
        section: ValidatedSection,
    ) -> Result<H2ByteStreamEvent<P>, ServerError> {
        let end_stream = flags & 0x1 != 0;
        if self.has_inbound(stream_id) {
            if !end_stream {
                self.endpoint.recycle_decoded_header_fields(headers);
                return Err(ServerError::InvalidFrame);
            }
            let limits = self.endpoint.http_limits;
            let result = match self.inbound_mut(stream_id) {
                Some(state) => state
                    .accept_trailers(section, limits)
                    .and_then(|()| state.finish()),
                None => Err(ServerError::InvalidFrame),
            };
            if let Err(error) = result {
                self.endpoint.recycle_decoded_header_fields(headers);
                return Err(error);
            }
            self.close_inbound(stream_id);
            Ok(H2ByteStreamEvent::Trailers { stream_id, headers })
        } else {
            if self
                .endpoint
                .outbound_shutdown
                .sent_last_stream_id()
                .is_some_and(|last_stream_id| stream_id > last_stream_id)
            {
                return Err(ServerError::InvalidFrame);
            }
            if stream_id.is_multiple_of(2)
                || stream_id <= self.max_peer_stream_id
                || self.knows_stream(stream_id)
                || self.active_stream_count() >= self.endpoint.limits.max_active_streams
            {
                return Err(ServerError::InvalidFrame);
            }
            self.max_peer_stream_id = stream_id;
            let inbound = if end_stream {
                H2StreamState::new_raw(
                    section,
                    self.endpoint.http_limits,
                    self.request_body_limit,
                )?
                .finish()?;
                None
            } else {
                Some(H2StreamState::new_raw(
                    section,
                    self.endpoint.http_limits,
                    self.request_body_limit,
                )?)
            };
            self.insert_server_stream(stream_id, inbound);
            self.max_processed_peer_stream_id = stream_id;
            Ok(H2ByteStreamEvent::RequestHeaders {
                stream_id,
                headers,
                end_stream,
            })
        }
    }

    /// Accepts fields decoded by an adapter-owned connection HPACK decoder.
    pub fn accept_external_header_fields(
        &mut self,
        stream_id: u32,
        flags: u8,
        headers: Vec<H2HeaderField>,
    ) -> Result<H2ByteStreamEvent, ServerError> {
        self.endpoint.ensure_external_hpack_adapter_ready()?;
        let disposition = self.validate_header_stream_state(stream_id, flags)?;
        let role = if self.has_inbound(stream_id) {
            H2HeaderValidationRole::Trailers
        } else {
            H2HeaderValidationRole::Request
        };
        let section = self.validate_external_header_fields(stream_id, role, &headers)?;
        if let H2HeaderStreamDisposition::Refuse = disposition {
            self.record_refused_stream(stream_id);
            return Err(ServerError::InvalidFrame);
        }
        self.event_from_decoded_header_fields(stream_id, flags & 0x5, headers, section)
    }

    pub fn accept_complete_header_block_bytes(
        &mut self,
        stream_id: u32,
        flags: u8,
        block: &[u8],
    ) -> Result<H2ByteStreamEvent, ServerError> {
        self.endpoint.validate_complete_header_block(block)?;
        self.event_from_complete_headers_parts::<Vec<u8>>(stream_id, flags & 0x5, block, false)
    }

    pub fn accept_complete_header_block(
        &mut self,
        stream_id: u32,
        flags: u8,
        block: &[u8],
    ) -> Result<H2StreamEvent, ServerError> {
        self.accept_complete_header_block_bytes(stream_id, flags, block)?
            .try_into_text()
            .map_err(|_| ServerError::MalformedMessage)
    }

    pub fn validate_data_frame(
        &mut self,
        stream_id: u32,
        payload_len: usize,
        end_stream: bool,
    ) -> Result<(), ServerError> {
        if stream_id == 0 {
            self.endpoint.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "invalid HTTP/2 frame",
            ));
            return Err(ServerError::InvalidFrame);
        }
        if self.is_reset_tolerant(stream_id) {
            return Ok(());
        }
        if self.endpoint.tombstones.contains_key(&stream_id)
            || self.endpoint.streams.contains_key(&stream_id)
        {
            if self.has_inbound(stream_id) {
                let state = self
                    .inbound_mut(stream_id)
                    .ok_or(ServerError::InvalidFrame)?;
                state.receive_data(payload_len, end_stream)?;
                if end_stream {
                    self.close_inbound(stream_id);
                }
                return Ok(());
            }
            self.endpoint.last_protocol_error = Some(H2ProtocolError::stream(
                stream_id,
                H2ErrorCode::StreamClosed,
                "DATA arrived after the stream closed",
            ));
            return Err(ServerError::InvalidFrame);
        }
        self.endpoint.last_protocol_error = Some(H2ProtocolError::connection(
            H2ErrorCode::ProtocolError,
            "DATA referenced an idle client stream",
        ));
        Err(ServerError::InvalidFrame)
    }

    /// Abandons a server-side stream and records reset-tolerant closure.
    ///
    /// A retained tombstone surfaces later in-flight DATA as
    /// [`H2StreamEvent::DiscardedData`] so the owner can charge connection
    /// flow control. A zero tombstone limit retains no reset tolerance.
    pub fn close_stream(&mut self, stream_id: u32) {
        self.forget_stream(stream_id, H2StreamTombstone::ResetTolerant);
    }

    pub(crate) fn active_stream_count(&self) -> usize {
        self.endpoint.streams.len()
    }

    pub fn finish_response_stream(&mut self, stream_id: u32) {
        self.close_outbound(stream_id);
    }

    pub(crate) fn finish_compat_frame_error(
        &mut self,
        protocol_error: H2ProtocolError,
    ) -> Result<Vec<u8>, ServerError> {
        let original = self.endpoint.last_compat_error.take();
        let recoverable = std::mem::take(&mut self.last_compat_error_recoverable);
        if recoverable && matches!(protocol_error.scope, H2ErrorScope::Stream(_)) {
            let compatibility = compatibility_error(original, protocol_error);
            return match self.handle_stream_error(protocol_error) {
                Ok(output) => {
                    self.handled_compat_error = Some(compatibility);
                    Ok(output)
                }
                Err(error) => {
                    self.reported_protocol_error =
                        Some(h2_error_from_server_error(error.clone(), None));
                    Err(error)
                }
            };
        }
        self.reported_protocol_error = Some(protocol_error);
        Err(compatibility_error(original, protocol_error))
    }

    pub(crate) fn handle_stream_error(
        &mut self,
        protocol_error: H2ProtocolError,
    ) -> Result<Vec<u8>, ServerError> {
        let H2ErrorScope::Stream(stream_id) = protocol_error.scope else {
            return Err(ServerError::InvalidFrame);
        };
        let output = self.rst_stream_frame_with_code(stream_id, protocol_error.code)?;
        if !stream_id.is_multiple_of(2) {
            self.max_peer_stream_id = self.max_peer_stream_id.max(stream_id);
        }
        self.close_stream(stream_id);
        self.handled_stream_error = Some(protocol_error);
        Ok(output)
    }

    pub(crate) fn h2_error_from_server_error(
        &mut self,
        error: ServerError,
        head: Option<H2FrameHead>,
    ) -> H2ProtocolError {
        if let Some(typed) = self.endpoint.last_protocol_error.take() {
            return typed;
        }
        let unclassified_connection_error = matches!(
            error,
            ServerError::NeedMore
                | ServerError::InvalidFrame
                | ServerError::InvalidPreface
                | ServerError::PeerGoaway { .. }
        );
        let is_hpack_error = matches!(
            error,
            ServerError::InvalidHpack | ServerError::UnsupportedHpack
        );
        let mut typed = h2_error_from_server_error(error, head);
        if unclassified_connection_error {
            typed.scope = H2ErrorScope::Connection;
        }
        if is_hpack_error && let Some(hpack_error) = self.endpoint.last_hpack_error.take() {
            typed = H2ProtocolError::hpack(typed.scope, hpack_error, typed.debug);
        }
        typed
    }

    pub(crate) fn enqueue_outbound_header_block(
        &mut self,
        stream_id: u32,
        fields: &[H2HeaderField],
        end_stream: bool,
        reserved_tail: usize,
        append_tail: impl FnOnce(&mut Vec<u8>),
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        self.endpoint.enqueue_outbound_header_block(
            stream_id,
            fields,
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
        self.endpoint.enqueue_outbound_header_block_by(
            stream_id,
            field_count,
            field_at,
            end_stream,
            reserved_tail,
            append_tail,
        )
    }

    pub fn response_frames(
        &mut self,
        stream_id: u32,
        status: u16,
        body: &[u8],
        end_stream: bool,
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        self.response_frames_with_headers(stream_id, status, &[], body, end_stream)
    }

    /// Assigns response HEADERS and DATA to the connection-owned outbound queue.
    ///
    /// Call [`Self::next_outbound_block`] to hand the complete transaction to
    /// the transport, then call [`Self::acknowledge_outbound_block`]. This
    /// commit-bearing return replaces the former freely reorderable `Vec<u8>`.
    pub fn response_frames_with_headers(
        &mut self,
        stream_id: u32,
        status: u16,
        headers: &[Header<'_>],
        body: &[u8],
        end_stream: bool,
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        self.response_frames_with_fields_by(
            stream_id,
            status,
            headers.len(),
            |index| str_header_as_raw(&headers[index]),
            body,
            end_stream,
        )
    }

    pub fn response_frames_with_raw_headers(
        &mut self,
        stream_id: u32,
        status: u16,
        headers: &[H2HeaderField],
        body: &[u8],
        end_stream: bool,
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        self.response_frames_with_fields_by(
            stream_id,
            status,
            headers.len(),
            |index| headers[index].as_ref(),
            body,
            end_stream,
        )
    }

    pub(crate) fn response_frames_with_fields_by<'a>(
        &mut self,
        stream_id: u32,
        status: u16,
        header_count: usize,
        header_at: impl Fn(usize) -> H2RawHeaderRef<'a> + Copy,
        body: &[u8],
        end_stream: bool,
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        if body.len() > self.endpoint.http_limits.max_body_bytes() {
            return Err(h2_body_limit_error(
                stream_id,
                self.endpoint.http_limits,
                body.len(),
            ));
        }
        let mut status_storage = [0; 20];
        let status = decimal_bytes(usize::from(status), &mut status_storage);
        let mut content_length_storage = [0; 20];
        let content_length = decimal_bytes(body.len(), &mut content_length_storage);
        let field_count = header_count
            .checked_add(2)
            .ok_or_else(|| outbound_hpack_error(H2HpackError::AllocationFailed))?;
        let field_at = |index| match index {
            0 => H2RawHeaderRef::new(b":status", status),
            1 => H2RawHeaderRef::new(b"content-length", content_length),
            _ => header_at(index - 2),
        };
        enforce_h2_outbound_field_limits(
            stream_id,
            field_count,
            field_at,
            self.endpoint.http_limits,
        )?;
        let max_frame_size = self.endpoint.settings.max_frame_size;
        self.enqueue_outbound_header_block_by(
            stream_id,
            field_count,
            field_at,
            body.is_empty() && end_stream,
            data_frames_encoded_len(body.len(), max_frame_size),
            |output| {
                encode_data_frames(stream_id, body, end_stream, max_frame_size, output);
            },
        )
    }

    pub fn response_headers_frame(
        &mut self,
        stream_id: u32,
        status: u16,
        headers: &[Header<'_>],
        end_stream: bool,
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        let body_length = content_length_from_fields_by(headers.len(), |index| {
            str_header_as_raw(&headers[index])
        })
        .ok()
        .flatten()
        .unwrap_or(0);
        self.response_headers_frame_with_fields_by(
            stream_id,
            status,
            headers.len(),
            |index| str_header_as_raw(&headers[index]),
            body_length,
            end_stream,
        )
    }

    pub fn response_headers_frame_with_raw_headers(
        &mut self,
        stream_id: u32,
        status: u16,
        headers: &[H2HeaderField],
        end_stream: bool,
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        let body_length = content_length_from_raw_headers(headers)
            .ok()
            .flatten()
            .unwrap_or(0);
        self.response_headers_frame_with_raw_headers_and_body_length(
            stream_id,
            status,
            headers,
            body_length,
            end_stream,
        )
    }

    /// Queues response headers after enforcing header and complete-body limits.
    pub fn response_headers_frame_with_raw_headers_and_body_length(
        &mut self,
        stream_id: u32,
        status: u16,
        headers: &[H2HeaderField],
        body_length: usize,
        end_stream: bool,
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        self.response_headers_frame_with_fields_by(
            stream_id,
            status,
            headers.len(),
            |index| headers[index].as_ref(),
            body_length,
            end_stream,
        )
    }

    pub(crate) fn response_headers_frame_with_fields_by<'a>(
        &mut self,
        stream_id: u32,
        status: u16,
        header_count: usize,
        header_at: impl Fn(usize) -> H2RawHeaderRef<'a> + Copy,
        body_length: usize,
        end_stream: bool,
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        if body_length > self.endpoint.http_limits.max_body_bytes() {
            return Err(h2_body_limit_error(
                stream_id,
                self.endpoint.http_limits,
                body_length,
            ));
        }
        let mut status_storage = [0; 20];
        let status = decimal_bytes(usize::from(status), &mut status_storage);
        let field_count = header_count
            .checked_add(1)
            .ok_or_else(|| outbound_hpack_error(H2HpackError::AllocationFailed))?;
        let field_at = |index| {
            if index == 0 {
                H2RawHeaderRef::new(b":status", status)
            } else {
                header_at(index - 1)
            }
        };
        enforce_h2_outbound_field_limits(
            stream_id,
            field_count,
            field_at,
            self.endpoint.http_limits,
        )?;
        self.enqueue_outbound_header_block_by(
            stream_id,
            field_count,
            field_at,
            end_stream,
            0,
            |_| {},
        )
    }

    /// Reports peer-advertised DATA capacity for an active response stream.
    pub fn send_capacity(
        &self,
        stream_id: u32,
        pending_bytes: usize,
    ) -> Result<H2SendCapacity, ServerError> {
        let half = self
            .outbound(stream_id)
            .ok_or(ServerError::FlowControlViolation)?;
        Ok(h2_send_capacity(
            half,
            self.endpoint.connection_send_available(),
            stream_id,
            pending_bytes,
        ))
    }

    /// Plans one DATA frame without copying its payload or mutating send state.
    pub fn prepare_data_frame(
        &self,
        stream_id: u32,
        pending_bytes: usize,
        end_stream: bool,
    ) -> Result<Option<H2DataFramePlan>, ServerError> {
        self.prepare_data_frame_inner(stream_id, pending_bytes, end_stream, false)
    }

    pub(crate) fn prepare_data_frame_before_trailers(
        &self,
        stream_id: u32,
        pending_bytes: usize,
    ) -> Result<Option<H2DataFramePlan>, ServerError> {
        self.prepare_data_frame_inner(stream_id, pending_bytes, false, true)
    }

    pub(crate) fn prepare_data_frame_inner(
        &self,
        stream_id: u32,
        pending_bytes: usize,
        end_stream: bool,
        allow_empty_nonterminal: bool,
    ) -> Result<Option<H2DataFramePlan>, ServerError> {
        let half = self
            .outbound(stream_id)
            .ok_or(ServerError::FlowControlViolation)?;
        h2_plan_data_frame(
            half,
            self.endpoint.connection_send_available(),
            self.endpoint.settings.max_frame_size,
            stream_id,
            pending_bytes,
            end_stream,
            allow_empty_nonterminal,
        )
    }

    /// Commits a successfully handed-off DATA frame to flow-control and stream state.
    pub fn commit_data_frame(&mut self, plan: H2DataFramePlan) -> Result<(), ServerError> {
        let half = self
            .outbound(plan.stream_id)
            .ok_or(ServerError::FlowControlViolation)?;
        h2_validate_data_plan(
            half,
            self.endpoint.connection_send_available(),
            self.endpoint.settings.max_frame_size,
            plan,
        )?;
        let outbound = self
            .outbound_mut(plan.stream_id)
            .ok_or(ServerError::FlowControlViolation)?;
        // Preserve server error side effects: stream credit, connection credit,
        // then body accounting.
        outbound.consume_window(plan.payload_len)?;
        self.endpoint.consume_connection_window(plan.payload_len)?;
        let outbound = self
            .outbound_mut(plan.stream_id)
            .ok_or(ServerError::FlowControlViolation)?;
        outbound.account_body(plan.payload_len)?;
        if plan.end_stream {
            self.finish_response_stream(plan.stream_id);
        }
        Ok(())
    }

    pub fn data_frame(&self, stream_id: u32, payload: &[u8], end_stream: bool) -> Vec<u8> {
        self.endpoint.data_frame(stream_id, payload, end_stream)
    }

    pub fn trailers_frame(
        &mut self,
        stream_id: u32,
        headers: &[Header<'_>],
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        self.endpoint.trailers_frame(stream_id, headers)
    }

    pub fn trailers_frame_with_raw_headers(
        &mut self,
        stream_id: u32,
        headers: &[H2HeaderField],
    ) -> Result<H2OutboundCommit, H2ProtocolError> {
        self.endpoint
            .trailers_frame_with_raw_headers(stream_id, headers)
    }

    pub(crate) fn decode_h2_header_fields(
        &mut self,
        stream_id: u32,
        block: &[u8],
        role: H2HeaderValidationRole,
    ) -> Result<(Vec<H2HeaderField>, ValidatedSection), ServerError> {
        self.endpoint
            .decode_h2_header_fields(stream_id, block, role)
    }

    pub(crate) fn bind_compact_header_fields(
        &self,
        section: ValidatedSection,
        role: H2HeaderValidationRole,
    ) -> Result<ValidatedHeaderSectionRef<'_>, ServerError> {
        self.endpoint.bind_compact_header_fields(section, role)
    }

    pub(crate) fn compact_header_block(&self) -> Result<H2RawHeaderBlockRef<'_>, ServerError> {
        self.endpoint.compact_header_block()
    }

    pub(crate) fn reset_compact_header_fields(&mut self) {
        self.endpoint.reset_compact_header_fields();
    }

    pub(crate) fn validate_external_header_fields(
        &mut self,
        stream_id: u32,
        role: H2HeaderValidationRole,
        headers: &[H2HeaderField],
    ) -> Result<ValidatedSection, ServerError> {
        self.endpoint
            .validate_external_header_fields(stream_id, role, headers)
    }

    pub fn discard_hpack_block(&mut self, block: &[u8]) -> Result<(), ServerError> {
        self.endpoint.discard_hpack_block(block)
    }

    pub(crate) fn apply_settings(
        &mut self,
        payload: &[u8],
    ) -> Result<Option<H2InitialWindowSizeChange>, ServerError> {
        self.endpoint.apply_settings(payload, |stream| {
            stream
                .outbound
                .as_mut()
                .map(|outbound| &mut outbound.window)
        })
    }

    pub(crate) fn encode_local_settings(&mut self, output: &mut Vec<u8>) {
        self.endpoint.encode_local_settings(output);
    }

    pub(crate) fn encode_local_connection_window_update(&self, output: &mut Vec<u8>) {
        self.endpoint.encode_local_connection_window_update(output);
    }
}
