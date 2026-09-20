//! HTTP/2 client connection state machine.

use crate::Header;
use crate::HttpLimits;
use crate::head::PreparedH2Request;
use crate::server::ServerError;
use crate::server::h2::endpoint::*;
use crate::server::h2::events::*;
use crate::server::h2::flow::*;
use crate::server::h2::headers::*;
use crate::server::h2::wire::*;

#[derive(Debug)]
pub(crate) enum H2ClientReceiveHalf {
    AwaitingHead,
    Body(H2StreamState),
}

pub(crate) struct H2ClientStream {
    pub(crate) send: Option<H2OutboundHalf>,
    pub(crate) receive: Option<H2ClientReceiveHalf>,
    pub(crate) request_is_head: bool,
    pub(crate) request_is_connect: bool,
    pub(crate) response_body_limit: Option<usize>,
}

pub struct H2Client {
    pub(crate) endpoint: H2Endpoint<H2ClientStream>,
    pub(crate) next_stream_id: u32,
    pub(crate) preface_sent: bool,
    push_disabled_ack: bool,
    highest_promised: u32,
    pending_push: Option<PendingPush>,
}

struct PendingPush {
    associated: u32,
    promised: u32,
    block: Vec<u8>,
    continuations: usize,
}

impl Default for H2Client {
    fn default() -> Self {
        Self::from_endpoint(H2Endpoint::default())
    }
}

impl H2Client {
    fn from_endpoint(mut endpoint: H2Endpoint<H2ClientStream>) -> Self {
        endpoint.local_enable_push = Some(false);
        Self {
            endpoint,
            next_stream_id: 1,
            preface_sent: false,
            push_disabled_ack: false,
            highest_promised: 0,
            pending_push: None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod prepared_request_tests {
    use std::cell::Cell;

    use super::*;
    use crate::head::prepare_h2_request;

    fn request_fields() -> [H2HeaderField; 2] {
        [
            H2HeaderField::new(b"content-length", b"4"),
            H2HeaderField::new(b"x-test", b"one").with_sensitive(true),
        ]
    }

    fn request_field_bytes(fields: &[H2HeaderField]) -> usize {
        [
            (b":method".as_slice(), b"POST".as_slice()),
            (b":scheme".as_slice(), b"https".as_slice()),
            (b":authority".as_slice(), b"example.test".as_slice()),
            (b":path".as_slice(), b"/items".as_slice()),
        ]
        .into_iter()
        .map(|(name, value)| crate::hpack_field_size(name, value))
        .chain(
            fields
                .iter()
                .map(|field| crate::hpack_field_size(&field.name, &field.value)),
        )
        .sum()
    }

    #[test]
    fn prepared_open_visits_regular_fields_once_across_prepare_and_encode() {
        let fields = request_fields();
        let prepared_visits = Cell::new(0);
        let prepared_field_at = |index: usize| {
            prepared_visits.set(prepared_visits.get() + 1);
            fields[index].as_ref()
        };
        let prepared = prepare_h2_request(
            "POST",
            "https",
            "example.test",
            "/items",
            fields.len(),
            prepared_field_at,
            HttpLimits::new(),
        )
        .unwrap();
        assert_eq!(prepared_visits.get(), 0);

        let mut trusted = H2Client::default();
        trusted
            .open_stream_with_prepared_request(prepared, false)
            .unwrap();
        let trusted_visits = prepared_visits.get();
        let trusted_wire = trusted.next_outbound_block().unwrap().bytes().to_vec();
        let trusted_diagnostics = trusted
            .endpoint
            .header_codecs
            .as_ref()
            .unwrap()
            .outbound
            .diagnostics();

        let raw_visits = Cell::new(0);
        let raw_field_at = |index: usize| {
            raw_visits.set(raw_visits.get() + 1);
            fields[index].as_ref()
        };
        let mut raw = H2Client::default();
        raw.open_stream_with_raw_header_refs(
            "POST",
            "https",
            "example.test",
            "/items",
            fields.len(),
            raw_field_at,
            false,
        )
        .unwrap();

        assert_eq!(trusted_visits, fields.len());
        assert!(raw_visits.get() > trusted_visits);
        assert_eq!(raw.next_outbound_block().unwrap().bytes(), trusted_wire);
        assert_eq!(
            raw.endpoint
                .header_codecs
                .as_ref()
                .unwrap()
                .outbound
                .diagnostics(),
            trusted_diagnostics
        );
    }

    #[test]
    fn prepared_and_raw_paths_enforce_body_and_header_limits() {
        let fields = request_fields();
        let body_limits = HttpLimits::new().set_max_body_bytes(3);
        let prepared = prepare_h2_request(
            "POST",
            "https",
            "example.test",
            "/items",
            fields.len(),
            |index| fields[index].as_ref(),
            body_limits,
        )
        .unwrap();
        let mut trusted = H2Client::with_local_flow_control_and_http_limits(
            H2Settings::default().initial_window_size,
            H2Settings::default().initial_window_size,
            H2Limits::default(),
            body_limits,
        )
        .unwrap();
        assert_eq!(
            trusted.open_stream_with_prepared_request(prepared, false),
            Err(H2PreparedRequestError::Protocol(h2_body_limit_error(
                1,
                body_limits,
                4,
            )))
        );

        let mut raw = H2Client::with_local_flow_control_and_http_limits(
            H2Settings::default().initial_window_size,
            H2Settings::default().initial_window_size,
            H2Limits::default(),
            body_limits,
        )
        .unwrap();
        assert!(
            raw.open_stream_with_raw_headers(
                "POST",
                "https",
                "example.test",
                "/items",
                &fields,
                false,
            )
            .is_err()
        );

        let prepared_header_limits = HttpLimits::new().set_max_headers(2);
        let prepared = prepare_h2_request(
            "POST",
            "https",
            "example.test",
            "/items",
            fields.len(),
            |index| fields[index].as_ref(),
            prepared_header_limits,
        );
        let mut trusted = H2Client::default();
        assert_eq!(
            trusted.open_stream_with_prepared_request(prepared.unwrap(), false),
            Err(H2PreparedRequestError::Request(
                ServerError::TooManyHeaders {
                    limit: 2,
                    actual: 3,
                }
            ))
        );
        let raw_header_limits = HttpLimits::new().set_max_headers(1);
        let mut raw = H2Client::with_local_flow_control_and_http_limits(
            H2Settings::default().initial_window_size,
            H2Settings::default().initial_window_size,
            H2Limits::default(),
            raw_header_limits,
        )
        .unwrap();
        assert!(
            raw.open_stream_with_raw_headers(
                "POST",
                "https",
                "example.test",
                "/items",
                &fields,
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn prepared_open_rechecks_fused_facts_against_destination_limits() {
        let fields = request_fields();
        let prepare = || {
            prepare_h2_request(
                "POST",
                "https",
                "example.test",
                "/items",
                fields.len(),
                |index| fields[index].as_ref(),
                HttpLimits::new(),
            )
            .unwrap()
        };

        let count_limits = HttpLimits::new().set_max_headers(0);
        let mut count_limited = H2Client::with_local_flow_control_and_http_limits(
            H2Settings::default().initial_window_size,
            H2Settings::default().initial_window_size,
            H2Limits::default(),
            count_limits,
        )
        .unwrap();
        assert_eq!(
            count_limited.open_stream_with_prepared_request(prepare(), false),
            Err(H2PreparedRequestError::Protocol(
                H2ProtocolError::resource_limit(
                    H2ErrorScope::Stream(1),
                    crate::HttpErrorKind::TooManyHeaders,
                    0,
                    3,
                    "HTTP/2 header count exceeds configured limit",
                )
            ))
        );

        let byte_limits = HttpLimits::new().set_max_header_bytes(0);
        let mut byte_limited = H2Client::with_local_flow_control_and_http_limits(
            H2Settings::default().initial_window_size,
            H2Settings::default().initial_window_size,
            H2Limits::default(),
            byte_limits,
        )
        .unwrap();
        let error = byte_limited
            .open_stream_with_prepared_request(prepare(), false)
            .unwrap_err();
        let H2PreparedRequestError::Protocol(error) = error else {
            panic!("destination byte limits produce a protocol limit error");
        };
        assert_eq!(error.scope, H2ErrorScope::Stream(1));
        assert_eq!(
            error.http_error_kind,
            Some(crate::HttpErrorKind::HeadersTooLarge)
        );
        assert_eq!(error.limit.unwrap().limit(), 0);
        assert_eq!(
            error.limit.unwrap().actual(),
            Some(request_field_bytes(&fields))
        );
    }

    #[test]
    fn preparation_errors_precede_stream_open_errors() {
        let invalid_fields = [H2HeaderField::new(b"X-Test", b"one")];
        let invalid = prepare_h2_request(
            "POST",
            "https",
            "example.test",
            "/items",
            invalid_fields.len(),
            |index| invalid_fields[index].as_ref(),
            HttpLimits::new(),
        )
        .unwrap();
        let mut invalid_client = H2Client::default();
        invalid_client.endpoint.received_goaway_last_stream_id = Some(0);
        assert_eq!(
            invalid_client.open_stream_with_prepared_request(invalid, false),
            Err(H2PreparedRequestError::Request(ServerError::InvalidHeader))
        );

        let fields = request_fields();
        let limited = prepare_h2_request(
            "POST",
            "https",
            "example.test",
            "/items",
            fields.len(),
            |index| fields[index].as_ref(),
            HttpLimits::new().set_max_headers(0),
        )
        .unwrap();
        let mut limited_client = H2Client::default();
        limited_client.endpoint.received_goaway_last_stream_id = Some(0);
        assert_eq!(
            limited_client.open_stream_with_prepared_request(limited, false),
            Err(H2PreparedRequestError::Request(
                ServerError::TooManyHeaders {
                    limit: 0,
                    actual: 3,
                }
            ))
        );

        let destination_limits = HttpLimits::new().set_max_headers(0);
        let destination_limited = prepare_h2_request(
            "POST",
            "https",
            "example.test",
            "/items",
            fields.len(),
            |index| fields[index].as_ref(),
            HttpLimits::new(),
        )
        .unwrap();
        let mut unavailable = H2Client::with_local_flow_control_and_http_limits(
            H2Settings::default().initial_window_size,
            H2Settings::default().initial_window_size,
            H2Limits::default(),
            destination_limits,
        )
        .unwrap();
        unavailable.endpoint.received_goaway_last_stream_id = Some(0);
        assert_eq!(
            unavailable.open_stream_with_prepared_request(destination_limited, false),
            Err(H2PreparedRequestError::Protocol(projection_error(0)))
        );
    }

    #[test]
    fn prepared_validation_rejects_semantic_errors_before_state_changes() {
        fn assert_rejected(fields: &[H2HeaderField], expected: ServerError) {
            let visits = Cell::new(0);
            let prepared = prepare_h2_request(
                "POST",
                "https",
                "example.test",
                "/items",
                fields.len(),
                |index| {
                    visits.set(visits.get() + 1);
                    fields[index].as_ref()
                },
                HttpLimits::new(),
            )
            .unwrap();
            assert_eq!(visits.get(), 0);

            let mut client = H2Client::default();
            let diagnostics_before = client
                .endpoint
                .header_codecs
                .as_ref()
                .unwrap()
                .outbound
                .diagnostics();
            assert_eq!(
                client.open_stream_with_prepared_request(prepared, false),
                Err(H2PreparedRequestError::Request(expected))
            );
            assert_eq!(visits.get(), fields.len());
            assert_eq!(client.next_stream_id, 1);
            assert!(client.endpoint.streams.is_empty());
            assert_eq!(client.endpoint.outbound_queue.next_sequence, 0);
            assert!(client.endpoint.outbound_queue.blocks.is_empty());
            assert_eq!(
                client
                    .endpoint
                    .header_codecs
                    .as_ref()
                    .unwrap()
                    .outbound
                    .diagnostics(),
                diagnostics_before
            );

            let valid = request_fields();
            let retry = prepare_h2_request(
                "POST",
                "https",
                "example.test",
                "/items",
                valid.len(),
                |index| valid[index].as_ref(),
                HttpLimits::new(),
            )
            .unwrap();
            client
                .open_stream_with_prepared_request(retry, false)
                .unwrap();
            let mut baseline = H2Client::default();
            let baseline_request = prepare_h2_request(
                "POST",
                "https",
                "example.test",
                "/items",
                valid.len(),
                |index| valid[index].as_ref(),
                HttpLimits::new(),
            )
            .unwrap();
            baseline
                .open_stream_with_prepared_request(baseline_request, false)
                .unwrap();
            assert_eq!(
                client.next_outbound_block().unwrap().bytes(),
                baseline.next_outbound_block().unwrap().bytes()
            );
        }

        assert_rejected(
            &[H2HeaderField::new(b"X-Test", b"one")],
            ServerError::InvalidHeader,
        );
        assert_rejected(
            &[H2HeaderField::new(b"x-test", b"one\n")],
            ServerError::InvalidHeader,
        );
        assert_rejected(
            &[H2HeaderField::new(b":status", b"200")],
            ServerError::InvalidHeader,
        );
        assert_rejected(
            &[H2HeaderField::new(b":method", b"GET")],
            ServerError::InvalidHeader,
        );
        assert_rejected(
            &[
                H2HeaderField::new(b"x-test", b"one"),
                H2HeaderField::new(b":path", b"/late"),
            ],
            ServerError::InvalidHeader,
        );
        for name in [
            b"connection".as_slice(),
            b"keep-alive",
            b"proxy-connection",
            b"transfer-encoding",
            b"upgrade",
        ] {
            assert_rejected(
                &[H2HeaderField::new(name, b"invalid")],
                ServerError::InvalidHeader,
            );
        }
        assert_rejected(
            &[H2HeaderField::new(b"te", b"gzip")],
            ServerError::InvalidHeader,
        );
        assert_rejected(
            &[H2HeaderField::new(b"content-length", b"four")],
            ServerError::InvalidContentLength,
        );
        assert_rejected(
            &[
                H2HeaderField::new(b"content-length", b"4"),
                H2HeaderField::new(b"content-length", b"5"),
            ],
            ServerError::InvalidContentLength,
        );
    }

    #[test]
    fn prepared_limit_rejection_is_transactional_and_retryable() {
        let fields = request_fields();
        let field_bytes = request_field_bytes(&fields);
        let preparation_limits = HttpLimits::new().set_max_header_bytes(field_bytes - 1);
        let prepared = prepare_h2_request(
            "POST",
            "https",
            "example.test",
            "/items",
            fields.len(),
            |index| fields[index].as_ref(),
            preparation_limits,
        )
        .unwrap();
        let mut client = H2Client::default();
        assert_eq!(
            client.open_stream_with_prepared_request(prepared, false),
            Err(H2PreparedRequestError::Request(
                ServerError::HeaderTooLarge {
                    limit: field_bytes - 1,
                    actual: field_bytes,
                }
            ))
        );
        assert_eq!(client.next_stream_id, 1);
        assert!(client.endpoint.streams.is_empty());
        assert_eq!(client.endpoint.outbound_queue.next_sequence, 0);
        assert!(client.endpoint.outbound_queue.blocks.is_empty());

        let retry = prepare_h2_request(
            "POST",
            "https",
            "example.test",
            "/items",
            fields.len(),
            |index| fields[index].as_ref(),
            HttpLimits::new(),
        )
        .unwrap();
        client
            .open_stream_with_prepared_request(retry, false)
            .unwrap();
        let mut baseline = H2Client::default();
        let baseline_request = prepare_h2_request(
            "POST",
            "https",
            "example.test",
            "/items",
            fields.len(),
            |index| fields[index].as_ref(),
            HttpLimits::new(),
        )
        .unwrap();
        baseline
            .open_stream_with_prepared_request(baseline_request, false)
            .unwrap();
        assert_eq!(
            client.next_outbound_block().unwrap().bytes(),
            baseline.next_outbound_block().unwrap().bytes()
        );
    }

    #[cfg(feature = "hpack-test-support")]
    #[test]
    fn prepared_validation_precedes_queue_and_encoder_allocation_failures() {
        let invalid_fields = [H2HeaderField::new(b"X-Test", b"one")];
        for reserve_queue in [false, true] {
            let visits = Cell::new(0);
            let prepared = prepare_h2_request(
                "POST",
                "https",
                "example.test",
                "/items",
                invalid_fields.len(),
                |index| {
                    visits.set(visits.get() + 1);
                    invalid_fields[index].as_ref()
                },
                HttpLimits::new(),
            )
            .unwrap();
            let mut client = H2Client::default();
            if reserve_queue {
                client
                    .endpoint
                    .outbound_queue
                    .blocks
                    .try_reserve(1)
                    .unwrap();
            }
            client.set_outbound_allocation_failure_after_for_testing(Some(0));

            assert_eq!(
                client.open_stream_with_prepared_request(prepared, false),
                Err(H2PreparedRequestError::Request(ServerError::InvalidHeader))
            );
            assert_eq!(visits.get(), invalid_fields.len());
            assert_eq!(client.next_stream_id, 1);
            assert!(client.endpoint.streams.is_empty());
            assert_eq!(client.endpoint.outbound_queue.next_sequence, 0);
            assert!(client.endpoint.outbound_queue.blocks.is_empty());
        }

        let valid_fields = request_fields();
        for reserve_queue in [false, true] {
            let visits = Cell::new(0);
            let prepared = prepare_h2_request(
                "POST",
                "https",
                "example.test",
                "/items",
                valid_fields.len(),
                |index| {
                    visits.set(visits.get() + 1);
                    valid_fields[index].as_ref()
                },
                HttpLimits::new().set_max_headers(0),
            )
            .unwrap();
            let mut client = H2Client::default();
            if reserve_queue {
                client
                    .endpoint
                    .outbound_queue
                    .blocks
                    .try_reserve(1)
                    .unwrap();
            }
            client.set_outbound_allocation_failure_after_for_testing(Some(0));

            assert_eq!(
                client.open_stream_with_prepared_request(prepared, false),
                Err(H2PreparedRequestError::Request(
                    ServerError::TooManyHeaders {
                        limit: 0,
                        actual: 3,
                    }
                ))
            );
            assert_eq!(visits.get(), valid_fields.len());
            assert_eq!(client.next_stream_id, 1);
            assert!(client.endpoint.streams.is_empty());
            assert_eq!(client.endpoint.outbound_queue.next_sequence, 0);
            assert!(client.endpoint.outbound_queue.blocks.is_empty());
        }
    }

    #[cfg(feature = "hpack-test-support")]
    #[test]
    fn prepared_allocation_failure_is_transactional_and_retryable() {
        let fields = request_fields();
        let prepare = || {
            prepare_h2_request(
                "POST",
                "https",
                "example.test",
                "/items",
                fields.len(),
                |index| fields[index].as_ref(),
                HttpLimits::new(),
            )
            .unwrap()
        };
        let mut baseline = H2Client::default();
        baseline
            .open_stream_with_prepared_request(prepare(), false)
            .unwrap();
        let expected = baseline.next_outbound_block().unwrap().bytes().to_vec();

        let mut client = H2Client::default();
        client
            .endpoint
            .outbound_queue
            .blocks
            .try_reserve(1)
            .unwrap();
        let diagnostics_before = client
            .endpoint
            .header_codecs
            .as_ref()
            .unwrap()
            .outbound
            .diagnostics();
        client.set_outbound_allocation_failure_after_for_testing(Some(0));
        assert_eq!(
            client.open_stream_with_prepared_request(prepare(), false),
            Err(H2PreparedRequestError::Protocol(outbound_hpack_error(
                H2HpackError::AllocationFailed,
            )))
        );
        assert_eq!(client.next_stream_id, 1);
        assert!(client.endpoint.streams.is_empty());
        assert_eq!(client.endpoint.outbound_queue.next_sequence, 0);
        assert!(client.endpoint.outbound_queue.blocks.is_empty());
        assert_eq!(
            client
                .endpoint
                .header_codecs
                .as_ref()
                .unwrap()
                .outbound
                .diagnostics(),
            diagnostics_before
        );

        client.set_outbound_allocation_failure_after_for_testing(None);
        client
            .open_stream_with_prepared_request(prepare(), false)
            .unwrap();
        assert_eq!(client.next_outbound_block().unwrap().bytes(), expected);
    }
}

impl H2Client {
    /// Creates protocol state for an adapter that owns the connection HPACK pair.
    ///
    /// Header blocks must be decoded by the adapter and supplied through
    /// [`Self::accept_external_header_fields`].
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

    pub fn inbound_hpack_diagnostics(&self) -> H2HpackDiagnosticsSnapshot {
        self.endpoint.inbound_hpack_diagnostics()
    }

    pub fn outbound_hpack_diagnostics(&self) -> H2HpackDiagnosticsSnapshot {
        self.endpoint.outbound_hpack_diagnostics()
    }

    /// Returns the next complete outbound transaction in connection wire order.
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

    /// Begins a reversible raw header-block transaction.
    pub fn prepare_outbound_header_block<'connection, 'headers>(
        &'connection mut self,
        stream_id: u32,
        headers: &'headers [H2HeaderField],
        end_stream: bool,
    ) -> Result<H2OutboundHeaderBlock<'connection, 'headers>, H2ProtocolError> {
        self.endpoint.ensure_not_terminal()?;
        Ok(H2OutboundHeaderBlock {
            target: H2OutboundHeaderBlockTarget::Client(self),
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
        self.endpoint.record_control(frame_type)
    }

    #[cfg(test)]
    pub(crate) fn has_send(&self, stream_id: u32) -> bool {
        self.endpoint
            .streams
            .get(&stream_id)
            .is_some_and(|stream| stream.send.is_some())
    }

    #[cfg(test)]
    pub(crate) fn send_window_available(&self, stream_id: u32) -> Option<i32> {
        self.endpoint
            .streams
            .get(&stream_id)
            .and_then(|stream| stream.send.as_ref())
            .map(|send| send.window.available())
    }

    pub(crate) fn has_receive_body(&self, stream_id: u32) -> bool {
        self.endpoint
            .streams
            .get(&stream_id)
            .is_some_and(|stream| matches!(stream.receive, Some(H2ClientReceiveHalf::Body(_))))
    }

    pub(crate) fn is_awaiting_head(&self, stream_id: u32) -> bool {
        self.endpoint
            .streams
            .get(&stream_id)
            .is_some_and(|stream| matches!(stream.receive, Some(H2ClientReceiveHalf::AwaitingHead)))
    }

    pub(crate) fn knows_stream(&self, stream_id: u32) -> bool {
        self.endpoint.knows_stream(stream_id)
    }

    pub(crate) fn is_reset_tolerant(&self, stream_id: u32) -> bool {
        self.endpoint.is_reset_tolerant(stream_id)
    }

    pub(crate) fn send(&self, stream_id: u32) -> Option<&H2OutboundHalf> {
        self.endpoint
            .streams
            .get(&stream_id)
            .and_then(|stream| stream.send.as_ref())
    }

    pub(crate) fn send_mut(&mut self, stream_id: u32) -> Option<&mut H2OutboundHalf> {
        self.endpoint
            .streams
            .get_mut(&stream_id)
            .and_then(|stream| stream.send.as_mut())
    }

    pub(crate) fn receive_body_mut(&mut self, stream_id: u32) -> Option<&mut H2StreamState> {
        self.endpoint
            .streams
            .get_mut(&stream_id)
            .and_then(|stream| match stream.receive.as_mut() {
                Some(H2ClientReceiveHalf::Body(state)) => Some(state),
                _ => None,
            })
    }

    pub(crate) fn insert_client_stream(
        &mut self,
        stream_id: u32,
        end_stream: bool,
        request_is_head: bool,
    ) {
        self.endpoint.streams.insert(
            stream_id,
            H2ClientStream {
                send: (!end_stream).then(|| {
                    H2OutboundHalf::new(
                        self.endpoint.settings.initial_window_size,
                        Some(self.endpoint.http_limits.max_body_bytes()),
                    )
                }),
                receive: Some(H2ClientReceiveHalf::AwaitingHead),
                request_is_head,
                request_is_connect: false,
                response_body_limit: Some(self.endpoint.http_limits.max_body_bytes()),
            },
        );
    }

    pub(crate) fn close_send(&mut self, stream_id: u32) {
        if let Some(stream) = self.endpoint.streams.get_mut(&stream_id) {
            stream.send = None;
        }
        self.retire_if_complete(stream_id, H2StreamTombstone::Closed);
    }

    pub(crate) fn finish_client_stream(&mut self, stream_id: u32) {
        self.endpoint
            .forget_stream(stream_id, H2StreamTombstone::Closed);
    }

    pub(crate) fn forget_stream(&mut self, stream_id: u32, kind: H2StreamTombstone) {
        self.endpoint.forget_stream(stream_id, kind);
    }

    pub(crate) fn retire_if_complete(&mut self, stream_id: u32, kind: H2StreamTombstone) {
        self.endpoint.retire_if_complete(stream_id, kind, |stream| {
            stream.send.is_none() && stream.receive.is_none()
        });
    }

    #[cfg(test)]
    pub(crate) fn assert_stream_invariants(&self) {
        for (stream_id, stream) in &self.endpoint.streams {
            assert!(
                stream.send.is_some() || stream.receive.is_some(),
                "active client stream {stream_id} has no live half"
            );
        }
        self.endpoint.assert_stream_bookkeeping();
    }

    pub(crate) fn apply_send_window_update(
        &mut self,
        stream_id: u32,
        increment: u32,
    ) -> Result<(), ServerError> {
        match self
            .endpoint
            .apply_send_window_update(stream_id, increment, |stream| {
                stream.send.as_mut().map(|half| &mut half.window)
            })? {
            H2SendWindowUpdateDisposition::Applied
            | H2SendWindowUpdateDisposition::IgnoredKnown => Ok(()),
            H2SendWindowUpdateDisposition::Unknown => Err(ServerError::InvalidFrame),
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
            graceful_shutdown_ping: false,
        }
    }

    pub const fn timer_intent(&self) -> Option<H2TimerIntent> {
        self.timer_obligations().primary_intent()
    }

    /// Emits connection GOAWAY with `SETTINGS_TIMEOUT` when ACK debt remains.
    pub fn settings_ack_timeout_elapsed(&mut self) -> Result<Vec<u8>, ServerError> {
        if !self.endpoint.local_settings_ack_debt.timer_owed() {
            return Ok(Vec::new());
        }
        self.endpoint.local_settings_ack_debt.note_timeout();
        self.goaway_frame(0, H2ErrorCode::SettingsTimeout.as_u32())
    }

    pub(crate) const fn received_goaway_last_stream_id(&self) -> Option<u32> {
        self.endpoint.received_goaway_last_stream_id
    }

    pub fn shutdown_intent(&self) -> H2ShutdownIntent {
        match self.endpoint.outbound_shutdown {
            H2OutboundShutdown::Open | H2OutboundShutdown::GracefulPingPending => {
                H2ShutdownIntent::None
            }
            H2OutboundShutdown::GoawaySent { last_stream_id }
                if !self.endpoint.streams.is_empty() =>
            {
                H2ShutdownIntent::Drain { last_stream_id }
            }
            H2OutboundShutdown::GoawaySent { .. } => H2ShutdownIntent::Close,
        }
    }

    pub fn goaway_frame(
        &mut self,
        last_stream_id: u32,
        error_code: u32,
    ) -> Result<Vec<u8>, ServerError> {
        self.endpoint.goaway_frame(last_stream_id, error_code)
    }

    pub fn connection_preface(&mut self) -> Vec<u8> {
        if self.preface_sent {
            return Vec::new();
        }
        self.preface_sent = true;
        let mut output = Vec::new();
        output.extend_from_slice(CLIENT_PREFACE);
        self.encode_local_settings(&mut output);
        self.encode_local_connection_window_update(&mut output);
        output
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

    pub fn open_stream(
        &mut self,
        method: &str,
        scheme: &str,
        authority: &str,
        path: &str,
        headers: &[Header<'_>],
        end_stream: bool,
    ) -> Result<(u32, H2OutboundCommit), ServerError> {
        self.open_stream_with_raw_header_refs(
            method,
            scheme,
            authority,
            path,
            headers.len(),
            |index| str_header_as_raw(&headers[index]),
            end_stream,
        )
        .map_err(ServerError::from)
    }

    pub fn open_stream_with_raw_headers(
        &mut self,
        method: &str,
        scheme: &str,
        authority: &str,
        path: &str,
        headers: &[H2HeaderField],
        end_stream: bool,
    ) -> Result<(u32, H2OutboundCommit), H2ProtocolError> {
        self.open_stream_with_raw_header_refs(
            method,
            scheme,
            authority,
            path,
            headers.len(),
            |index| headers[index].as_ref(),
            end_stream,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_stream_with_raw_header_refs<'a>(
        &mut self,
        method: &str,
        scheme: &str,
        authority: &str,
        path: &str,
        header_count: usize,
        header_at: impl Fn(usize) -> H2RawHeaderRef<'a> + Copy,
        end_stream: bool,
    ) -> Result<(u32, H2OutboundCommit), H2ProtocolError> {
        self.open_stream_with_raw_header_refs_inner(
            method,
            scheme,
            authority,
            path,
            header_count,
            header_at,
            end_stream,
        )
    }

    pub(crate) fn open_stream_with_prepared_request<'a, F>(
        &mut self,
        prepared: PreparedH2Request<'a, F>,
        end_stream: bool,
    ) -> Result<(u32, H2OutboundCommit), H2PreparedRequestError>
    where
        F: Fn(usize) -> H2RawHeaderRef<'a> + Copy,
    {
        let (method, scheme, authority, path, header_count, header_at, preparation_limits) =
            prepared.into_parts();
        let field_count = header_count
            .checked_add(4)
            .ok_or_else(|| outbound_hpack_error(H2HpackError::AllocationFailed))
            .map_err(H2PreparedRequestError::Protocol)?;
        let field_at = |index| match index {
            0 => H2RawHeaderRef::new(b":method", method.as_bytes()),
            1 => H2RawHeaderRef::new(b":scheme", scheme.as_bytes()),
            2 => H2RawHeaderRef::new(b":authority", authority.as_bytes()),
            3 => H2RawHeaderRef::new(b":path", path.as_bytes()),
            _ => header_at(index - 4),
        };
        let stream_id = match self.next_stream_for_open() {
            Ok(stream_id) => stream_id,
            Err(_) => {
                validate_prepared_request_for_preparation_by(
                    field_count,
                    field_at,
                    preparation_limits,
                )?;
                return Err(H2PreparedRequestError::Protocol(projection_error(0)));
            }
        };
        if self.endpoint.streams.try_reserve(1).is_err() {
            validate_prepared_request_for_preparation_by(
                field_count,
                field_at,
                preparation_limits,
            )?;
            return Err(H2PreparedRequestError::Protocol(outbound_hpack_error(
                H2HpackError::AllocationFailed,
            )));
        }
        let commit = self.endpoint.enqueue_prepared_request_header_block_by(
            stream_id,
            field_count,
            field_at,
            end_stream,
            preparation_limits,
        )?;
        self.next_stream_id = self
            .next_stream_id
            .checked_add(2)
            .expect("validated stream ID has a successor");
        self.insert_client_stream(stream_id, end_stream, method.eq_ignore_ascii_case("HEAD"));
        Ok((stream_id, commit))
    }

    #[allow(clippy::too_many_arguments)]
    fn open_stream_with_raw_header_refs_inner<'a>(
        &mut self,
        method: &str,
        scheme: &str,
        authority: &str,
        path: &str,
        header_count: usize,
        header_at: impl Fn(usize) -> H2RawHeaderRef<'a> + Copy,
        end_stream: bool,
    ) -> Result<(u32, H2OutboundCommit), H2ProtocolError> {
        let stream_id = self
            .next_stream_for_open()
            .map_err(|_| projection_error(0))?;
        self.endpoint
            .streams
            .try_reserve(1)
            .map_err(|_| outbound_hpack_error(H2HpackError::AllocationFailed))?;
        let field_count = header_count
            .checked_add(4)
            .ok_or_else(|| outbound_hpack_error(H2HpackError::AllocationFailed))?;
        let field_at = |index| match index {
            0 => H2RawHeaderRef::new(b":method", method.as_bytes()),
            1 => H2RawHeaderRef::new(b":scheme", scheme.as_bytes()),
            2 => H2RawHeaderRef::new(b":authority", authority.as_bytes()),
            3 => H2RawHeaderRef::new(b":path", path.as_bytes()),
            _ => header_at(index - 4),
        };
        enforce_h2_outbound_field_limits(
            stream_id,
            field_count,
            field_at,
            self.endpoint.http_limits,
        )?;
        let body_length = content_length_from_fields_by(header_count, header_at)
            .ok()
            .flatten()
            .unwrap_or(0);
        if body_length > self.endpoint.http_limits.max_body_bytes() {
            return Err(h2_body_limit_error(
                stream_id,
                self.endpoint.http_limits,
                body_length,
            ));
        }
        let commit = self.enqueue_outbound_header_block_by(
            stream_id,
            field_count,
            field_at,
            end_stream,
            0,
            |_| {},
        )?;
        self.next_stream_id = self
            .next_stream_id
            .checked_add(2)
            .expect("validated stream ID has a successor");
        self.insert_client_stream(stream_id, end_stream, method.eq_ignore_ascii_case("HEAD"));
        Ok((stream_id, commit))
    }

    pub fn reserve_stream(&mut self) -> Result<u32, ServerError> {
        let stream_id = self.next_stream_for_open()?;
        self.next_stream_id = self
            .next_stream_id
            .checked_add(2)
            .ok_or(ServerError::InvalidFrame)?;
        self.insert_client_stream(stream_id, false, false);
        Ok(stream_id)
    }

    pub(crate) fn next_stream_for_open(&self) -> Result<u32, ServerError> {
        let stream_id = self.next_stream_id;
        if stream_id == 0 || stream_id > 0x7fff_ffff {
            return Err(ServerError::InvalidFrame);
        }
        if self.endpoint.received_goaway_last_stream_id.is_some() {
            return Err(ServerError::InvalidFrame);
        }
        let active_stream_limit = (self.endpoint.settings.max_concurrent_streams as usize)
            .min(self.endpoint.limits.max_active_streams);
        if self.endpoint.streams.len() >= active_stream_limit {
            return Err(ServerError::InvalidFrame);
        }
        Ok(stream_id)
    }

    pub(crate) fn active_stream_limit(&self) -> usize {
        (self.endpoint.settings.max_concurrent_streams as usize)
            .min(self.endpoint.limits.max_active_streams)
    }

    pub(crate) fn can_open_stream(&self) -> bool {
        self.next_stream_for_open().is_ok()
    }

    pub(crate) fn reset_stream(
        &mut self,
        stream_id: u32,
        error_code: H2ErrorCode,
    ) -> Result<Vec<u8>, ServerError> {
        if stream_id == 0 || stream_id > 0x7fff_ffff {
            return Err(ServerError::InvalidFrame);
        }
        let mut output = Vec::new();
        H2Frame {
            frame_type: H2FrameType::RstStream,
            flags: 0,
            stream_id,
            payload: error_code.as_u32().to_be_bytes().to_vec(),
        }
        .encode(&mut output);
        self.close_stream(stream_id);
        Ok(output)
    }

    pub(crate) fn stream_body(
        &mut self,
        stream_id: u32,
        request: bool,
        response: bool,
    ) -> Result<(), ServerError> {
        let stream = self
            .endpoint
            .streams
            .get_mut(&stream_id)
            .ok_or(ServerError::InvalidOutboundState)?;
        if request && let Some(send) = stream.send.as_mut() {
            send.sent.limit = None;
        }
        if response {
            stream.response_body_limit = None;
        }
        Ok(())
    }

    /// Reports peer-advertised DATA capacity for an open request stream.
    pub fn send_capacity(
        &self,
        stream_id: u32,
        pending_bytes: usize,
    ) -> Result<H2SendCapacity, ServerError> {
        let half = self
            .send(stream_id)
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
        let half = self
            .send(stream_id)
            .ok_or(ServerError::FlowControlViolation)?;
        h2_plan_data_frame(
            half,
            self.endpoint.connection_send_available(),
            self.endpoint.settings.max_frame_size,
            stream_id,
            pending_bytes,
            end_stream,
            false,
        )
    }

    /// Commits a successfully handed-off DATA frame to flow-control and stream state.
    pub fn commit_data_frame(&mut self, plan: H2DataFramePlan) -> Result<(), ServerError> {
        let half = self
            .send(plan.stream_id)
            .ok_or(ServerError::FlowControlViolation)?;
        h2_validate_data_plan(
            half,
            self.endpoint.connection_send_available(),
            self.endpoint.settings.max_frame_size,
            plan,
        )?;
        let stream = self
            .send_mut(plan.stream_id)
            .ok_or(ServerError::FlowControlViolation)?;
        // Preserve client error side effects: stream credit and body accounting
        // precede connection-credit consumption.
        stream.consume_window(plan.payload_len)?;
        stream.account_body(plan.payload_len)?;
        self.endpoint.consume_connection_window(plan.payload_len)?;
        if plan.end_stream {
            self.close_send(plan.stream_id);
        }
        self.endpoint.record_progress_frame();
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

    pub fn accept_bytes(
        &mut self,
        input: &[u8],
    ) -> Result<(Option<H2ByteClientEvent>, usize, Vec<u8>), ServerError> {
        let (event, consumed, output) = self.accept_bytes_ref(input)?;
        Ok((
            event.map(H2ByteClientEventRef::into_owned),
            consumed,
            output,
        ))
    }

    pub fn accept_bytes_ref<'a>(
        &mut self,
        input: &'a [u8],
    ) -> Result<(Option<H2ByteClientEventRef<'a>>, usize, Vec<u8>), ServerError> {
        if let Some(error) = self.endpoint.terminal_protocol_error {
            return Err(error.into());
        }
        let mut output = Vec::new();
        let (frame, consumed) = match H2FrameRef::decode_with_max_frame_size(
            input,
            self.endpoint.limits.max_frame_size,
        ) {
            Ok(decoded) => decoded,
            Err(ServerError::NeedMore) => return Ok((None, 0, output)),
            Err(error) => return Err(error),
        };
        if self.pending_push.is_some() || frame.frame_type == H2FrameType::PushPromise {
            let output = self.accept_disabled_push(frame)?;
            return Ok((None, consumed, output));
        }
        match self.accept_frame_bytes_ref_typed(frame) {
            (H2FrameOutcome::Event(event), mut event_output) => {
                output.append(&mut event_output);
                Ok((Some(event), consumed, output))
            }
            (H2FrameOutcome::Ignored, mut event_output) => {
                output.append(&mut event_output);
                Ok((None, consumed, output))
            }
            (H2FrameOutcome::Error(error), _) => Err(compatibility_error(
                self.endpoint.last_compat_error.take(),
                error,
            )),
        }
    }

    pub(crate) fn accept_driver_bytes_ref<'a>(
        &mut self,
        input: &'a [u8],
    ) -> Result<(Option<H2DriverClientEvent<'a>>, usize, Vec<u8>), ServerError> {
        if let Some(error) = self.endpoint.terminal_protocol_error {
            return Err(error.into());
        }
        let (frame, consumed) = match H2FrameRef::decode_with_max_frame_size(
            input,
            self.endpoint.limits.max_frame_size,
        ) {
            Ok(decoded) => decoded,
            Err(ServerError::NeedMore) => return Ok((None, 0, Vec::new())),
            Err(error) => return Err(error),
        };
        if self.pending_push.is_some() || frame.frame_type == H2FrameType::PushPromise {
            let output = self.accept_disabled_push(frame)?;
            return Ok((None, consumed, output));
        }
        if !matches!(
            frame.frame_type,
            H2FrameType::Headers | H2FrameType::Continuation
        ) {
            let (outcome, output) = self.accept_frame_bytes_ref_typed(frame);
            return match outcome {
                H2FrameOutcome::Event(event) => Ok((
                    Some(H2DriverClientEvent::from_non_header(event)),
                    consumed,
                    output,
                )),
                H2FrameOutcome::Ignored => Ok((None, consumed, output)),
                H2FrameOutcome::Error(error) => {
                    self.endpoint.last_protocol_error = Some(error);
                    Err(compatibility_error(
                        self.endpoint.last_compat_error.take(),
                        error,
                    ))
                }
            };
        }
        self.endpoint.last_compat_error = None;
        if !self.endpoint.settings_seen {
            return Err(ServerError::InvalidFrame);
        }
        if let Some(pending) = self.endpoint.header_block.pending.as_ref()
            && (!matches!(frame.frame_type, H2FrameType::Continuation)
                || frame.stream_id != pending.stream_id)
        {
            return Err(ServerError::InvalidFrame);
        }
        let head = frame.head();
        match self.accept_driver_header_frame(frame) {
            Ok(event) => Ok((event, consumed, Vec::new())),
            Err(ServerError::NeedMore) => Ok((None, consumed, Vec::new())),
            Err(error) => {
                self.endpoint.last_compat_error = Some(error.clone());
                let typed = self.h2_error_from_server_error(error, Some(head));
                self.endpoint.last_protocol_error = Some(typed);
                Err(compatibility_error(
                    self.endpoint.last_compat_error.take(),
                    typed,
                ))
            }
        }
    }

    fn accept_disabled_push(&mut self, frame: H2FrameRef<'_>) -> Result<Vec<u8>, ServerError> {
        let invalid = || {
            H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "invalid PUSH_PROMISE or push after SETTINGS acknowledgment",
            )
        };
        if !self.endpoint.settings_seen || self.endpoint.header_block.pending.is_some() {
            self.endpoint.last_protocol_error = Some(invalid());
            return Err(ServerError::InvalidFrame);
        }
        let (promised, block) = if let Some(mut pending) = self.pending_push.take() {
            if frame.frame_type != H2FrameType::Continuation
                || frame.stream_id != pending.associated
            {
                self.endpoint.last_protocol_error = Some(invalid());
                return Err(ServerError::InvalidFrame);
            }
            pending.continuations += 1;
            if pending.continuations > self.endpoint.limits.max_continuation_frames
                || frame.payload.len()
                    > self
                        .endpoint
                        .limits
                        .max_encoded_header_block_size
                        .saturating_sub(pending.block.len())
            {
                self.endpoint.last_protocol_error = Some(invalid());
                return Err(ServerError::InvalidFrame);
            }
            pending.block.extend_from_slice(frame.payload);
            if frame.flags & 4 == 0 {
                self.pending_push = Some(pending);
                return Ok(Vec::new());
            }
            (pending.promised, std::borrow::Cow::Owned(pending.block))
        } else {
            if self.push_disabled_ack
                || frame.stream_id == 0
                || !self.endpoint.streams.contains_key(&frame.stream_id)
            {
                self.endpoint.last_protocol_error = Some(invalid());
                return Err(ServerError::InvalidFrame);
            }
            let payload = strip_padding(frame.flags, frame.payload)?;
            if payload.len() < 4 {
                self.endpoint.last_protocol_error = Some(H2ProtocolError::connection(
                    H2ErrorCode::FrameSizeError,
                    "PUSH_PROMISE lacks promised stream identifier",
                ));
                return Err(ServerError::InvalidFrame);
            }
            let promised =
                u32::from_be_bytes(payload[..4].try_into().expect("four bytes")) & 0x7fff_ffff;
            if promised == 0 || !promised.is_multiple_of(2) || promised <= self.highest_promised {
                self.endpoint.last_protocol_error = Some(invalid());
                return Err(ServerError::InvalidFrame);
            }
            self.highest_promised = promised;
            let block = &payload[4..];
            if block.len() > self.endpoint.limits.max_encoded_header_block_size {
                self.endpoint.last_protocol_error = Some(invalid());
                return Err(ServerError::InvalidFrame);
            }
            if frame.flags & 4 == 0 {
                self.pending_push = Some(PendingPush {
                    associated: frame.stream_id,
                    promised,
                    block: block.to_vec(),
                    continuations: 0,
                });
                return Ok(Vec::new());
            }
            (promised, std::borrow::Cow::Borrowed(block))
        };
        self.discard_hpack_block(&block)?;
        self.endpoint
            .remember_tombstone(promised, H2StreamTombstone::ResetTolerant);
        self.reset_stream(promised, H2ErrorCode::Cancel)
    }
    fn accept_driver_header_frame<'a>(
        &mut self,
        frame: H2FrameRef<'_>,
    ) -> Result<Option<H2DriverClientEvent<'a>>, ServerError> {
        let Some(block) = accept_header_frame_for_connection(
            &mut self.endpoint.header_block,
            frame,
            self.endpoint.limits,
            &mut self.endpoint.last_protocol_error,
            &mut self.endpoint.terminal_protocol_error,
        )?
        else {
            return Ok(None);
        };
        if self.is_reset_tolerant(block.stream_id) {
            self.discard_hpack_block(&block.block)?;
            return Ok(None);
        }
        self.event_from_complete_headers_driver(block).map(Some)
    }

    fn event_from_complete_headers_driver<'a>(
        &mut self,
        block: H2CompleteHeaderBlock<'_>,
    ) -> Result<H2DriverClientEvent<'a>, ServerError> {
        let stream_id = block.stream_id;
        let flags = block.flags;
        let role = if self.has_receive_body(stream_id) {
            H2HeaderValidationRole::Trailers
        } else {
            H2HeaderValidationRole::Response
        };
        let headers =
            self.endpoint
                .decode_compact_h2_header_fields(stream_id, &block.block, role)?;
        if block.self_dependency {
            self.endpoint.last_protocol_error = Some(H2ProtocolError::stream(
                stream_id,
                H2ErrorCode::ProtocolError,
                "HEADERS priority dependency references its own stream",
            ));
            return Err(ServerError::InvalidFrame);
        }
        self.validate_header_stream_state(stream_id, flags)?;
        let event = self.event_from_compact_header_fields(stream_id, flags, headers)?;
        self.record_progress_frame();
        Ok(event)
    }

    fn event_from_compact_header_fields<'a>(
        &mut self,
        stream_id: u32,
        flags: u8,
        section: ValidatedSection,
    ) -> Result<H2DriverClientEvent<'a>, ServerError> {
        if self.has_receive_body(stream_id) {
            if flags & 0x1 == 0 {
                return Err(ServerError::InvalidFrame);
            }
            let limits = self.endpoint.http_limits;
            match self.receive_body_mut(stream_id) {
                Some(state) => state
                    .accept_trailers(section, limits)
                    .and_then(|()| state.finish())?,
                None => return Err(ServerError::InvalidFrame),
            }
            self.finish_client_stream(stream_id);
            Ok(H2DriverClientEvent::Trailers { stream_id })
        } else {
            if !self.is_awaiting_head(stream_id) {
                return Err(ServerError::InvalidFrame);
            }
            let status = section
                .response_status
                .ok_or(ServerError::MalformedMessage)?;
            let informational = (100..=199).contains(&status);
            if informational {
                if flags & 0x1 != 0 {
                    return Err(ServerError::InvalidFrame);
                }
            } else {
                let (request_is_head, body_limit) = self
                    .endpoint
                    .streams
                    .get(&stream_id)
                    .map(|stream| (stream.request_is_head, stream.response_body_limit))
                    .ok_or(ServerError::InvalidFrame)?;
                let mut response_section = section;
                let tunnel = self.endpoint.streams.get(&stream_id).is_some_and(|stream| {
                    stream.request_is_connect && (200..300).contains(&status)
                });
                if tunnel {
                    response_section.content_length = None;
                }
                let state = H2StreamState::new_response_raw(
                    response_section,
                    request_is_head,
                    status,
                    self.endpoint.http_limits,
                    if tunnel { None } else { body_limit },
                )?;
                if flags & 0x1 == 0 {
                    if let Some(stream) = self.endpoint.streams.get_mut(&stream_id) {
                        stream.receive = Some(H2ClientReceiveHalf::Body(state));
                    }
                } else {
                    state.finish()?;
                    self.finish_client_stream(stream_id);
                }
            }
            Ok(H2DriverClientEvent::ResponseHeaders {
                stream_id,
                section,
                end_stream: flags & 0x1 != 0,
            })
        }
    }

    pub fn accept(
        &mut self,
        input: &[u8],
    ) -> Result<(Option<H2ClientEvent>, usize, Vec<u8>), ServerError> {
        let (event, consumed, output) = self.accept_bytes(input)?;
        let event = event
            .map(H2ByteClientEvent::try_into_text)
            .transpose()
            .map_err(|_| ServerError::MalformedMessage)?;
        Ok((event, consumed, output))
    }

    pub fn accept_ref<'a>(
        &mut self,
        input: &'a [u8],
    ) -> Result<(Option<H2ClientEventRef<'a>>, usize, Vec<u8>), ServerError> {
        let (event, consumed, output) = self.accept_bytes_ref(input)?;
        let event = event
            .map(H2ByteClientEvent::try_into_text)
            .transpose()
            .map_err(|_| ServerError::MalformedMessage)?;
        Ok((event, consumed, output))
    }

    pub fn accept_frame_bytes(
        &mut self,
        frame: H2Frame,
    ) -> Result<(H2ByteClientEvent, Vec<u8>), ServerError> {
        let (outcome, output) = self.accept_frame_bytes_typed(frame);
        match outcome {
            H2FrameOutcome::Event(event) => Ok((event, output)),
            H2FrameOutcome::Ignored => Err(ServerError::InvalidFrame),
            H2FrameOutcome::Error(error) => Err(compatibility_error(
                self.endpoint.last_compat_error.take(),
                error,
            )),
        }
    }

    pub fn accept_frame_bytes_ref<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<(H2ByteClientEventRef<'a>, Vec<u8>), ServerError> {
        let (outcome, output) = self.accept_frame_bytes_ref_typed(frame);
        match outcome {
            H2FrameOutcome::Event(event) => Ok((event, output)),
            H2FrameOutcome::Ignored => Err(ServerError::InvalidFrame),
            H2FrameOutcome::Error(error) => Err(compatibility_error(
                self.endpoint.last_compat_error.take(),
                error,
            )),
        }
    }

    pub fn accept_frame_bytes_typed(
        &mut self,
        frame: H2Frame,
    ) -> (H2FrameOutcome<H2ByteClientEvent>, Vec<u8>) {
        let (outcome, output) = self.accept_frame_bytes_ref_typed(frame.as_ref());
        (outcome.map_event(H2ByteClientEventRef::into_owned), output)
    }

    pub fn accept_frame_bytes_ref_typed<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> (H2FrameOutcome<H2ByteClientEventRef<'a>>, Vec<u8>) {
        self.endpoint.last_compat_error = None;
        if let Some(error) = self.endpoint.terminal_protocol_error {
            return (H2FrameOutcome::Error(error), Vec::new());
        }
        if self.endpoint.header_block.pending.is_some()
            && (!matches!(frame.frame_type, H2FrameType::Continuation)
                || self
                    .endpoint
                    .header_block
                    .pending
                    .as_ref()
                    .is_some_and(|pending| frame.stream_id != pending.stream_id))
        {
            return (
                H2FrameOutcome::Error(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "HTTP/2 header block continuation sequence violated",
                )),
                Vec::new(),
            );
        }
        if !self.endpoint.settings_seen
            && (!matches!(frame.frame_type, H2FrameType::Settings)
                || frame.flags & 0x1 != 0
                || frame.stream_id != 0)
        {
            return (
                H2FrameOutcome::Error(H2ProtocolError::connection(
                    H2ErrorCode::ProtocolError,
                    "HTTP/2 server connection preface must start with SETTINGS",
                )),
                Vec::new(),
            );
        }
        if frame.frame_type.is_unknown() {
            return (H2FrameOutcome::Ignored, Vec::new());
        }
        if matches!(frame.frame_type, H2FrameType::Priority) {
            let head = frame.head();
            if let Err(error) = self.record_control(H2FrameType::Priority) {
                return (
                    H2FrameOutcome::Error(h2_error_from_server_error(error, Some(head))),
                    Vec::new(),
                );
            }
            return if priority_payload(frame.stream_id, frame.payload).is_ok() {
                (H2FrameOutcome::Ignored, Vec::new())
            } else {
                (
                    H2FrameOutcome::Error(h2_error_from_server_error(
                        ServerError::InvalidFrame,
                        Some(head),
                    )),
                    Vec::new(),
                )
            };
        }
        let head = frame.head();
        match self.accept_frame_compat(frame) {
            Ok((event, output)) => (H2FrameOutcome::Event(event), output),
            Err(ServerError::NeedMore) => (H2FrameOutcome::Ignored, Vec::new()),
            Err(error) => {
                self.endpoint.last_compat_error = Some(error.clone());
                let typed = self.h2_error_from_server_error(error, Some(head));
                (H2FrameOutcome::Error(typed), Vec::new())
            }
        }
    }

    pub fn accept_frame(
        &mut self,
        frame: H2Frame,
    ) -> Result<(H2ClientEvent, Vec<u8>), ServerError> {
        let (outcome, output) = self.accept_frame_typed(frame);
        match outcome {
            H2FrameOutcome::Event(event) => Ok((event, output)),
            H2FrameOutcome::Ignored => Err(ServerError::InvalidFrame),
            H2FrameOutcome::Error(error) => Err(error.into()),
        }
    }

    pub fn accept_frame_ref<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<(H2ClientEventRef<'a>, Vec<u8>), ServerError> {
        let (outcome, output) = self.accept_frame_ref_typed(frame);
        match outcome {
            H2FrameOutcome::Event(event) => Ok((event, output)),
            H2FrameOutcome::Ignored => Err(ServerError::InvalidFrame),
            H2FrameOutcome::Error(error) => Err(error.into()),
        }
    }

    pub fn accept_frame_typed(
        &mut self,
        frame: H2Frame,
    ) -> (H2FrameOutcome<H2ClientEvent>, Vec<u8>) {
        let stream_id = frame.stream_id;
        let (outcome, output) = self.accept_frame_bytes_typed(frame);
        (project_client_outcome(outcome, stream_id), output)
    }

    pub fn accept_frame_ref_typed<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> (H2FrameOutcome<H2ClientEventRef<'a>>, Vec<u8>) {
        let stream_id = frame.stream_id;
        let (outcome, output) = self.accept_frame_bytes_ref_typed(frame);
        (project_client_outcome(outcome, stream_id), output)
    }

    pub(crate) fn accept_frame_compat<'a>(
        &mut self,
        frame: H2FrameRef<'a>,
    ) -> Result<(H2ByteClientEventRef<'a>, Vec<u8>), ServerError> {
        if self.endpoint.header_block.pending.is_some()
            && !matches!(frame.frame_type, H2FrameType::Continuation)
        {
            return Err(ServerError::InvalidFrame);
        }
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
                    self.push_disabled_ack = true;
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
                    H2Frame {
                        frame_type: H2FrameType::Settings,
                        flags: 0x1,
                        stream_id: 0,
                        payload: Vec::new(),
                    }
                    .encode(&mut output);
                    initial_window_size
                };
                H2ByteClientEvent::Settings {
                    initial_window_size,
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
                if !ack {
                    H2FrameRef {
                        frame_type: H2FrameType::Ping,
                        flags: 0x1,
                        stream_id: 0,
                        payload: frame.payload,
                    }
                    .encode(&mut output);
                }
                H2ByteClientEvent::Ping { ack }
            }
            H2FrameType::WindowUpdate => {
                self.record_control(H2FrameType::WindowUpdate)?;
                let increment = window_update_increment(frame.payload)?;
                self.apply_send_window_update(frame.stream_id, increment)?;
                H2ByteClientEvent::WindowUpdate {
                    stream_id: frame.stream_id,
                    increment,
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
                    return Err(ServerError::NeedMore);
                }
            }
            H2FrameType::Data => {
                let payload = data_payload(frame.flags, frame.payload)?;
                let discard = self.is_reset_tolerant(frame.stream_id);
                self.validate_data_frame(frame.stream_id, payload.len(), frame.flags & 0x1 != 0)?;
                if !payload.is_empty() || frame.flags & 0x1 != 0 {
                    self.record_progress_frame();
                }
                if discard {
                    H2ByteClientEvent::DiscardedData {
                        stream_id: frame.stream_id,
                        flow_control_len: frame.payload.len(),
                    }
                } else {
                    H2ByteClientEvent::Data {
                        stream_id: frame.stream_id,
                        payload,
                        flow_control_len: frame.payload.len(),
                        end_stream: frame.flags & 0x1 != 0,
                    }
                }
            }
            H2FrameType::RstStream => {
                self.record_control(H2FrameType::RstStream)?;
                if frame.stream_id == 0 || frame.payload.len() != 4 {
                    return Err(ServerError::InvalidFrame);
                }
                if !self.knows_stream(frame.stream_id) {
                    return Err(ServerError::InvalidFrame);
                }
                if self.endpoint.tombstones.contains_key(&frame.stream_id) {
                    return Err(ServerError::NeedMore);
                }
                self.forget_stream(frame.stream_id, H2StreamTombstone::ResetTolerant);
                self.endpoint.control_diagnostics.resets =
                    self.endpoint.control_diagnostics.resets.saturating_add(1);
                H2ByteClientEvent::Reset {
                    stream_id: frame.stream_id,
                    error_code: u32::from_be_bytes([
                        frame.payload[0],
                        frame.payload[1],
                        frame.payload[2],
                        frame.payload[3],
                    ]),
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
                // RFC 9113 §6.8 permits a second GOAWAY to narrow the set of
                // processed streams, but never to widen it again.
                // `graceful_http2_goaway_retries_only_unprocessed_stream`
                // pins the narrowing behavior at the adapter boundary.
                last_stream_id = self
                    .endpoint
                    .received_goaway_last_stream_id
                    .map_or(last_stream_id, |previous| previous.min(last_stream_id));
                self.endpoint.received_goaway_last_stream_id = Some(last_stream_id);
                self.endpoint.control_diagnostics.goaways =
                    self.endpoint.control_diagnostics.goaways.saturating_add(1);
                H2ByteClientEvent::Goaway {
                    last_stream_id,
                    error_code: u32::from_be_bytes([
                        frame.payload[4],
                        frame.payload[5],
                        frame.payload[6],
                        frame.payload[7],
                    ]),
                }
            }
            H2FrameType::Priority => {
                self.record_control(H2FrameType::Priority)?;
                return Err(ServerError::InvalidFrame);
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
                    return Err(ServerError::NeedMore);
                }
            }
            H2FrameType::PushPromise | H2FrameType::Unknown(_) => {
                return Err(ServerError::InvalidFrame);
            }
        };
        Ok((event, output))
    }

    pub(crate) fn apply_settings(
        &mut self,
        payload: &[u8],
    ) -> Result<Option<H2InitialWindowSizeChange>, ServerError> {
        if payload
            .as_chunks::<6>()
            .0
            .iter()
            .any(|setting| setting[..2] == [0, 2])
        {
            self.endpoint.last_protocol_error = Some(H2ProtocolError::connection(
                H2ErrorCode::ProtocolError,
                "a server sent SETTINGS_ENABLE_PUSH",
            ));
            return Err(ServerError::InvalidFrame);
        }
        self.endpoint.apply_settings(payload, |stream| {
            stream.send.as_mut().map(|send| &mut send.window)
        })
    }

    pub(crate) fn encode_local_settings(&mut self, output: &mut Vec<u8>) {
        self.endpoint.encode_local_settings(output);
    }

    pub(crate) fn encode_local_connection_window_update(&self, output: &mut Vec<u8>) {
        self.endpoint.encode_local_connection_window_update(output);
    }

    pub(crate) fn event_from_complete_headers<'a>(
        &mut self,
        block: H2CompleteHeaderBlock<'a>,
    ) -> Result<H2ByteClientEventRef<'a>, ServerError> {
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
    ) -> Result<H2ByteClientEvent<P>, ServerError> {
        let role = if self.has_receive_body(stream_id) {
            H2HeaderValidationRole::Trailers
        } else {
            H2HeaderValidationRole::Response
        };
        let (headers, section) = self.decode_h2_header_fields(stream_id, block, role)?;
        if self_dependency {
            self.endpoint.last_protocol_error = Some(H2ProtocolError::stream(
                stream_id,
                H2ErrorCode::ProtocolError,
                "HEADERS priority dependency references its own stream",
            ));
            self.endpoint.recycle_decoded_header_fields(headers);
            return Err(ServerError::InvalidFrame);
        }
        if let Err(error) = self.validate_header_stream_state(stream_id, flags) {
            self.endpoint.recycle_decoded_header_fields(headers);
            return Err(error);
        }
        self.event_from_decoded_header_fields(stream_id, flags, headers, section)
    }

    pub(crate) fn validate_header_stream_state(
        &self,
        stream_id: u32,
        flags: u8,
    ) -> Result<(), ServerError> {
        if self.has_receive_body(stream_id) {
            return if flags & 0x1 != 0 {
                Ok(())
            } else {
                Err(ServerError::InvalidFrame)
            };
        }
        if self.is_awaiting_head(stream_id) {
            Ok(())
        } else {
            Err(ServerError::InvalidFrame)
        }
    }

    pub(crate) fn event_from_decoded_header_fields<P>(
        &mut self,
        stream_id: u32,
        flags: u8,
        headers: Vec<H2HeaderField>,
        section: ValidatedSection,
    ) -> Result<H2ByteClientEvent<P>, ServerError> {
        if self.has_receive_body(stream_id) {
            if flags & 0x1 == 0 {
                self.endpoint.recycle_decoded_header_fields(headers);
                return Err(ServerError::InvalidFrame);
            }
            let limits = self.endpoint.http_limits;
            let result = match self.receive_body_mut(stream_id) {
                Some(state) => state
                    .accept_trailers(section, limits)
                    .and_then(|()| state.finish()),
                None => Err(ServerError::InvalidFrame),
            };
            if let Err(error) = result {
                self.endpoint.recycle_decoded_header_fields(headers);
                return Err(error);
            }
            self.finish_client_stream(stream_id);
            Ok(H2ByteClientEvent::Trailers { stream_id, headers })
        } else {
            if !self.is_awaiting_head(stream_id) {
                return Err(ServerError::InvalidFrame);
            }
            let status = section
                .response_status
                .ok_or(ServerError::MalformedMessage)?;
            let informational = (100..=199).contains(&status);
            if informational {
                if flags & 0x1 != 0 {
                    return Err(ServerError::InvalidFrame);
                }
            } else {
                let (request_is_head, body_limit) = self
                    .endpoint
                    .streams
                    .get(&stream_id)
                    .map(|stream| (stream.request_is_head, stream.response_body_limit))
                    .ok_or(ServerError::InvalidFrame)?;
                let mut response_section = section;
                let tunnel = self.endpoint.streams.get(&stream_id).is_some_and(|stream| {
                    stream.request_is_connect && (200..300).contains(&status)
                });
                if tunnel {
                    response_section.content_length = None;
                }
                let state = H2StreamState::new_response_raw(
                    response_section,
                    request_is_head,
                    status,
                    self.endpoint.http_limits,
                    if tunnel { None } else { body_limit },
                )?;
                if flags & 0x1 == 0 {
                    if let Some(stream) = self.endpoint.streams.get_mut(&stream_id) {
                        stream.receive = Some(H2ClientReceiveHalf::Body(state));
                    }
                } else {
                    state.finish()?;
                    self.finish_client_stream(stream_id);
                }
            }
            Ok(H2ByteClientEvent::ResponseHeaders {
                stream_id,
                headers,
                end_stream: flags & 0x1 != 0,
            })
        }
    }

    /// Accepts fields decoded by an adapter-owned connection HPACK decoder.
    pub fn accept_external_header_fields(
        &mut self,
        stream_id: u32,
        flags: u8,
        headers: Vec<H2HeaderField>,
    ) -> Result<H2ByteClientEvent, ServerError> {
        self.endpoint.ensure_external_hpack_adapter_ready()?;
        self.validate_header_stream_state(stream_id, flags)?;
        let role = if self.has_receive_body(stream_id) {
            H2HeaderValidationRole::Trailers
        } else {
            H2HeaderValidationRole::Response
        };
        let section = self.validate_external_header_fields(stream_id, role, &headers)?;
        self.event_from_decoded_header_fields(stream_id, flags & 0x5, headers, section)
    }

    pub fn accept_complete_header_block_bytes(
        &mut self,
        stream_id: u32,
        flags: u8,
        block: &[u8],
    ) -> Result<H2ByteClientEvent, ServerError> {
        self.endpoint.validate_complete_header_block(block)?;
        self.event_from_complete_headers_parts::<Vec<u8>>(stream_id, flags & 0x5, block, false)
    }

    pub fn accept_complete_header_block(
        &mut self,
        stream_id: u32,
        flags: u8,
        block: &[u8],
    ) -> Result<H2ClientEvent, ServerError> {
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
            return Err(ServerError::InvalidFrame);
        }
        if self.endpoint.tombstones.contains_key(&stream_id) {
            if self.is_reset_tolerant(stream_id) {
                return Ok(());
            }
            self.endpoint.last_protocol_error = Some(H2ProtocolError::stream(
                stream_id,
                H2ErrorCode::StreamClosed,
                "DATA arrived after the stream closed",
            ));
            return Err(ServerError::InvalidFrame);
        }
        let Some(state) = self.receive_body_mut(stream_id) else {
            return Err(ServerError::InvalidFrame);
        };
        state.receive_data(payload_len, end_stream)?;
        if end_stream {
            self.finish_client_stream(stream_id);
        }
        Ok(())
    }

    /// Abandons an active client stream and records reset-tolerant closure.
    ///
    /// Closing an unknown stream or a stream with a normal completion
    /// tombstone is a no-op. A retained tombstone for an active stream
    /// surfaces later in-flight DATA as [`H2ClientEvent::DiscardedData`] so
    /// the owner can charge connection flow control.
    pub fn close_stream(&mut self, stream_id: u32) {
        if self.endpoint.tombstones.contains_key(&stream_id)
            || !self.endpoint.streams.contains_key(&stream_id)
        {
            return;
        }
        self.forget_stream(stream_id, H2StreamTombstone::ResetTolerant);
    }

    pub(crate) fn h2_error_from_server_error(
        &mut self,
        error: ServerError,
        head: Option<H2FrameHead>,
    ) -> H2ProtocolError {
        if let Some(typed) = self.endpoint.last_protocol_error.take() {
            return typed;
        }
        let is_hpack_error = matches!(
            error,
            ServerError::InvalidHpack | ServerError::UnsupportedHpack
        );
        let mut typed = h2_error_from_server_error(error, head);
        if is_hpack_error && let Some(hpack_error) = self.endpoint.last_hpack_error.take() {
            typed = H2ProtocolError::hpack(typed.scope, hpack_error, typed.debug);
        }
        typed
    }

    pub fn classify_frame(&mut self, frame: H2Frame) -> Result<H2ClientEvent, ServerError> {
        match self.accept_frame_typed(frame) {
            (H2FrameOutcome::Event(event), _) => Ok(event),
            (H2FrameOutcome::Ignored, _) => Err(ServerError::InvalidFrame),
            (H2FrameOutcome::Error(error), _) => Err(error.into()),
        }
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
}
