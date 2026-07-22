//! I/O-independent HTTP protocol state machines.
//!
//! This crate owns HTTP protocol progress but performs no socket, file, timer, or
//! runtime I/O. Callers drain outbound bytes to their chosen transport, feed
//! inbound bytes from that transport, and acknowledge consumed bytes after each
//! event.
//!
//! [`Http1HeaderScratch`] lends HTTP/1 parser storage together with the current
//! input through a callback-scoped lifetime. Adapters process or copy borrowed
//! events in that callback, then may compact or refill their input without
//! retaining parser references.
//!
//! The initial surface is intentionally low-level and content-length oriented.
//! Request metadata and upload chunks are borrowed from caller-owned buffers, and
//! response body chunks borrow from caller-owned input. That shape keeps copying
//! visible to adapters that later map the state machine into Tokio, `kimojio`, or
//! direct single-threaded drivers.

#![forbid(unsafe_code)]

mod client;
mod driver;
mod error;
mod head;
mod hpack;
mod http1_scratch;
mod huffman_table;
mod limits;
mod persistence;
mod response_plan;
mod server;

pub use httparse::Header as ParseHeader;

pub use client::{
    ClientEvent as Http1ClientEvent, ClientState, Error, Header, HttpClient, Outbound, RequestHead,
    ResponseHead,
};
pub use driver::{
    BodyChunk, ClientConnection, ClientEvent, ClientIdleStatus, ClientRequest, ConnectionResponse,
    ExchangeId, HandledStreamError, HeaderBlock, HeaderIter, HeaderRef, HttpProtocol, HttpVersion,
    ProtocolSelection, RequestBodyMode, RequestExpectation, ServerConnection, ServerEvent, Step,
};
pub use error::{HttpErrorInfo, HttpErrorKind, HttpErrorScope, LimitViolation};
pub use head::{
    H2RequestHeadRef, H2ResponseHeadRef, Http1Version, http1_version, project_h2_request_head,
    project_h2_response_head,
};
pub use http1_scratch::{Http1HeaderScratch, MAX_SCRATCH_HEADERS};
pub use limits::{
    DEFAULT_MAX_REQUESTS_PER_CONNECTION, HttpLimits, hpack_entry_overhead, hpack_field_size,
    parse_content_length,
};
pub use persistence::{connection_header_value_has_token, http1_request_is_persistent};
pub use response_plan::{
    Http1BodyFraming, Http1ResponseContext, Http1ResponseParts, Http1ResponsePlan,
    is_framing_header,
};
pub use server::{
    CLIENT_PREFACE, H2_DEFAULT_MAX_ACTIVE_STREAMS, H2_SCHEDULER_SELECTION_BUCKET_UPPER_BOUNDS,
    H2BackpressureState, H2ByteClientEvent, H2ByteClientEventRef, H2ByteStreamEvent,
    H2ByteStreamEventRef, H2Client, H2ClientEvent, H2ClientEventRef, H2ControlDiagnostics,
    H2DataFramePlan, H2DecodeOutcome, H2ErrorCode, H2ErrorScope, H2FairStreamScheduler,
    H2FairStreamSchedulerDiagnostics, H2FlowControlWindow, H2FlowDiagnostics,
    H2FlowDiagnosticsSnapshot, H2FlowStall, H2Frame, H2FrameHead, H2FrameOutcome, H2FrameRef,
    H2FrameRefDecodeOutcome, H2FrameType, H2Header, H2HeaderBlockDecoder, H2HeaderBlockEncoder,
    H2HeaderField, H2HeaderProjectionError, H2HeaderRole, H2HpackDiagnosticsSnapshot,
    H2HpackEffectiveness, H2HpackError, H2Limits, H2OutboundBlockRef, H2OutboundCommit,
    H2OutboundHeaderBlock, H2ProtocolError, H2RawHeader, H2RawHeaderRef, H2ReceiveWindow,
    H2ReceiveWindowDiagnostics, H2Request, H2SendCapacity, H2Server, H2Setting, H2SettingId,
    H2Settings, H2SettingsSyncState, H2ShutdownIntent, H2StreamEvent, H2StreamEventRef,
    H2TimerIntent, Http1BodyKind, Http1ChunkedBody, Http1ChunkedEvent, Http1Codec,
    Http1ConnectionDecoder, Http1ConnectionEvent, Http1MessageHead, Http1MessageRole,
    Http1RequestHead, Http1ResponseHead, Http1Server, ServerError, ServerRequest, ServerResponse,
    project_h2_header_fields, project_h2_header_fields_for_role,
};

pub const EMPTY_HEADER: ParseHeader<'static> = httparse::EMPTY_HEADER;

#[cfg(feature = "hpack-test-support")]
/// Deterministic codec instrumentation available only with test support enabled.
pub mod hpack_test_support {
    use super::{H2Client, H2HeaderBlockDecoder, H2HeaderBlockEncoder, H2HpackError, H2Server};

    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct Diagnostics {
        pub encoded_blocks: u64,
        pub decoded_blocks: u64,
        pub indexed_fields: u64,
        pub incremental_fields: u64,
        pub without_indexing_fields: u64,
        pub never_indexed_fields: u64,
        pub huffman_strings: u64,
        pub plain_strings: u64,
        pub table_size_updates: u64,
        pub table_insertions: u64,
        pub table_evictions: u64,
        pub compression_errors: u64,
        pub header_list_too_large: u64,
        pub field_bytes: u64,
        pub wire_bytes: u64,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct TableSnapshot {
        pub max_size: usize,
        pub size: usize,
        pub entries: Vec<(Vec<u8>, Vec<u8>)>,
        pub container_capacities: [usize; 3],
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct PostLimitAllocationSnapshot {
        pub discarded_output_allocations: usize,
        pub dynamic_table_synchronization_allocations: usize,
    }

    fn diagnostics(value: crate::hpack::Diagnostics) -> Diagnostics {
        Diagnostics {
            encoded_blocks: value.encoded_blocks,
            decoded_blocks: value.decoded_blocks,
            indexed_fields: value.indexed_fields,
            incremental_fields: value.incremental_fields,
            without_indexing_fields: value.without_indexing_fields,
            never_indexed_fields: value.never_indexed_fields,
            huffman_strings: value.huffman_strings,
            plain_strings: value.plain_strings,
            table_size_updates: value.table_size_updates,
            table_insertions: value.table_insertions,
            table_evictions: value.table_evictions,
            compression_errors: value.compression_errors,
            header_list_too_large: value.header_list_too_large,
            field_bytes: value.field_bytes,
            wire_bytes: value.wire_bytes,
        }
    }

    fn table_snapshot(value: crate::hpack::TestTableSnapshot) -> TableSnapshot {
        TableSnapshot {
            max_size: value.max_size,
            size: value.size,
            entries: value.entries,
            container_capacities: value.container_capacities,
        }
    }

    pub fn fail_encoder_allocation_after(
        encoder: &mut H2HeaderBlockEncoder,
        successful_allocations: Option<usize>,
    ) {
        encoder.set_allocation_failure_after_for_testing(successful_allocations);
    }

    pub fn fail_decoder_allocation_after(
        decoder: &mut H2HeaderBlockDecoder,
        successful_allocations: Option<usize>,
    ) {
        decoder.set_allocation_failure_after_for_testing(successful_allocations);
    }

    pub fn fail_server_inbound_allocation_after(
        server: &mut H2Server,
        successful_allocations: Option<usize>,
    ) {
        server.set_inbound_allocation_failure_after_for_testing(successful_allocations);
    }

    pub fn fail_server_outbound_allocation_after(
        server: &mut H2Server,
        successful_allocations: Option<usize>,
    ) {
        server.set_outbound_allocation_failure_after_for_testing(successful_allocations);
    }

    pub fn fail_server_assembly_allocation_after(
        server: &mut H2Server,
        successful_allocations: Option<usize>,
    ) {
        server.set_assembly_allocation_failure_after_for_testing(successful_allocations);
    }

    pub fn set_server_pending_encoded_header_block_len(server: &mut H2Server, encoded_len: usize) {
        server.set_pending_encoded_header_block_len_for_testing(encoded_len);
    }

    pub fn fail_client_inbound_allocation_after(
        client: &mut H2Client,
        successful_allocations: Option<usize>,
    ) {
        client.set_inbound_allocation_failure_after_for_testing(successful_allocations);
    }

    pub fn fail_client_outbound_allocation_after(
        client: &mut H2Client,
        successful_allocations: Option<usize>,
    ) {
        client.set_outbound_allocation_failure_after_for_testing(successful_allocations);
    }

    pub fn fail_client_assembly_allocation_after(
        client: &mut H2Client,
        successful_allocations: Option<usize>,
    ) {
        client.set_assembly_allocation_failure_after_for_testing(successful_allocations);
    }

    pub fn set_client_pending_encoded_header_block_len(client: &mut H2Client, encoded_len: usize) {
        client.set_pending_encoded_header_block_len_for_testing(encoded_len);
    }

    pub fn encoder_diagnostics(encoder: &mut H2HeaderBlockEncoder) -> Diagnostics {
        diagnostics(encoder.test_diagnostics())
    }

    pub fn decoder_diagnostics(decoder: &H2HeaderBlockDecoder) -> Diagnostics {
        diagnostics(decoder.test_diagnostics())
    }

    pub fn encoder_table(encoder: &mut H2HeaderBlockEncoder) -> TableSnapshot {
        table_snapshot(encoder.test_table_snapshot())
    }

    pub fn decoder_table(decoder: &H2HeaderBlockDecoder) -> TableSnapshot {
        table_snapshot(decoder.test_table_snapshot())
    }

    pub fn server_inbound_table(server: &H2Server) -> TableSnapshot {
        table_snapshot(server.inbound_table_for_testing())
    }

    pub fn server_outbound_table(server: &H2Server) -> TableSnapshot {
        table_snapshot(server.outbound_table_for_testing())
    }

    pub fn client_inbound_table(client: &H2Client) -> TableSnapshot {
        table_snapshot(client.inbound_table_for_testing())
    }

    pub fn client_outbound_table(client: &H2Client) -> TableSnapshot {
        table_snapshot(client.outbound_table_for_testing())
    }

    pub fn saturate_server_hpack_diagnostics(server: &mut H2Server) {
        server.saturate_hpack_diagnostics_for_testing();
    }

    pub fn saturate_client_hpack_diagnostics(client: &mut H2Client) {
        client.saturate_hpack_diagnostics_for_testing();
    }

    pub fn server_connection_is_terminal(server: &H2Server) -> bool {
        server.terminal_for_testing()
    }

    pub fn client_connection_is_terminal(client: &H2Client) -> bool {
        client.terminal_for_testing()
    }

    pub fn encoder_configured_capacity(encoder: &mut H2HeaderBlockEncoder) -> usize {
        encoder.test_configured_max_size()
    }

    pub fn decoder_configured_capacity(decoder: &H2HeaderBlockDecoder) -> usize {
        decoder.test_configured_max_size()
    }

    pub fn decoder_post_limit_allocations(
        decoder: &H2HeaderBlockDecoder,
    ) -> Option<PostLimitAllocationSnapshot> {
        decoder
            .test_post_limit_allocations()
            .map(|observed| PostLimitAllocationSnapshot {
                discarded_output_allocations: observed.discarded_output_allocations,
                dynamic_table_synchronization_allocations: observed
                    .dynamic_table_synchronization_allocations,
            })
    }

    pub fn decode_integer(
        bytes: &[u8],
        prefix_bits: u8,
        width: u32,
    ) -> (Result<u128, H2HpackError>, usize) {
        let (result, consumed) = crate::hpack::test_decode_integer(bytes, prefix_bits, width);
        (result.map_err(Into::into), consumed)
    }

    pub fn account_lengths(
        name_len: usize,
        value_len: usize,
        limit: usize,
        initial_total: usize,
    ) -> (usize, bool) {
        crate::hpack::test_account_lengths(name_len, value_len, limit, initial_total)
    }

    pub fn encode_huffman(input: &[u8]) -> Vec<u8> {
        crate::hpack::test_encode_huffman(input)
    }
}
