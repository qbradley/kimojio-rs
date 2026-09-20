//! HTTP/2 frame codec, settings, limits, and protocol errors.

use crate::HttpLimits;
use crate::server::ServerError;

pub const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

pub(crate) const H2_MIN_MAX_FRAME_SIZE: usize = 16_384;
pub(crate) const H2_MAX_MAX_FRAME_SIZE: usize = 16_777_215;
pub(crate) const H2_MAX_WINDOW_SIZE: u32 = 2_147_483_647;
pub(crate) const H2_DEFAULT_MAX_HEADER_LIST_SIZE: usize = 64 * 1024;
/// Default active-stream limit for network-facing FSM HTTP owners.
///
/// Owners may explicitly configure a higher bounded limit. The fair scheduler
/// uses FxHash for stream IDs. Those IDs are monotonic protocol integers, not
/// attacker-controlled string keys.
pub const H2_DEFAULT_MAX_ACTIVE_STREAMS: usize = crate::limits::DEFAULT_MAX_ACTIVE_STREAMS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H2FrameType {
    Data,
    Headers,
    Priority,
    RstStream,
    Settings,
    PushPromise,
    Ping,
    Goaway,
    WindowUpdate,
    Continuation,
    Unknown(u8),
}

impl H2FrameType {
    /// Converts a raw HTTP/2 frame type byte into a known frame type.
    pub fn from_u8(value: u8) -> Result<Self, ServerError> {
        match value {
            0 => Ok(Self::Data),
            1 => Ok(Self::Headers),
            2 => Ok(Self::Priority),
            3 => Ok(Self::RstStream),
            4 => Ok(Self::Settings),
            5 => Ok(Self::PushPromise),
            6 => Ok(Self::Ping),
            7 => Ok(Self::Goaway),
            8 => Ok(Self::WindowUpdate),
            9 => Ok(Self::Continuation),
            _ => Err(ServerError::InvalidFrame),
        }
    }

    /// Converts a raw HTTP/2 frame type byte, preserving unknown extension types.
    pub const fn from_raw(value: u8) -> Self {
        match value {
            0 => Self::Data,
            1 => Self::Headers,
            2 => Self::Priority,
            3 => Self::RstStream,
            4 => Self::Settings,
            5 => Self::PushPromise,
            6 => Self::Ping,
            7 => Self::Goaway,
            8 => Self::WindowUpdate,
            9 => Self::Continuation,
            _ => Self::Unknown(value),
        }
    }

    /// Returns the wire type byte.
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Data => 0,
            Self::Headers => 1,
            Self::Priority => 2,
            Self::RstStream => 3,
            Self::Settings => 4,
            Self::PushPromise => 5,
            Self::Ping => 6,
            Self::Goaway => 7,
            Self::WindowUpdate => 8,
            Self::Continuation => 9,
            Self::Unknown(value) => value,
        }
    }

    /// Returns whether this is an unknown extension frame type.
    pub const fn is_unknown(self) -> bool {
        matches!(self, Self::Unknown(_))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct H2Frame {
    pub frame_type: H2FrameType,
    pub flags: u8,
    pub stream_id: u32,
    pub payload: Vec<u8>,
}

/// A decoded HTTP/2 frame that borrows its payload from the input buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2FrameRef<'a> {
    pub frame_type: H2FrameType,
    pub flags: u8,
    pub stream_id: u32,
    pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2FrameHead {
    pub frame_type: H2FrameType,
    pub flags: u8,
    pub stream_id: u32,
    pub payload_len: usize,
}

impl H2Frame {
    pub fn as_ref(&self) -> H2FrameRef<'_> {
        H2FrameRef {
            frame_type: self.frame_type,
            flags: self.flags,
            stream_id: self.stream_id,
            payload: &self.payload,
        }
    }

    pub fn encode(&self, output: &mut Vec<u8>) {
        self.as_ref().encode(output);
    }

    pub fn encode_header(
        frame_type: H2FrameType,
        flags: u8,
        stream_id: u32,
        payload_len: usize,
        output: &mut Vec<u8>,
    ) {
        output.push(((payload_len >> 16) & 0xff) as u8);
        output.push(((payload_len >> 8) & 0xff) as u8);
        output.push((payload_len & 0xff) as u8);
        output.push(frame_type.as_u8());
        output.push(flags);
        output.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
    }

    pub fn decode_header(input: &[u8]) -> Result<H2FrameHead, ServerError> {
        if input.len() < 9 {
            return Err(ServerError::NeedMore);
        }
        let payload_len =
            ((input[0] as usize) << 16) | ((input[1] as usize) << 8) | input[2] as usize;
        let frame_type = H2FrameType::from_raw(input[3]);
        let flags = input[4];
        let stream_id = u32::from_be_bytes([input[5], input[6], input[7], input[8]]) & 0x7fff_ffff;
        Ok(H2FrameHead {
            frame_type,
            flags,
            stream_id,
            payload_len,
        })
    }

    pub fn decode(input: &[u8]) -> Result<(Self, usize), ServerError> {
        Self::decode_outcome(input).into_result()
    }

    pub fn decode_with_max_frame_size(
        input: &[u8],
        max_frame_size: usize,
    ) -> Result<(Self, usize), ServerError> {
        Self::decode_outcome_with_max_frame_size(input, max_frame_size).into_result()
    }

    pub fn decode_outcome(input: &[u8]) -> H2DecodeOutcome {
        Self::decode_outcome_with_max_frame_size(input, H2_MAX_MAX_FRAME_SIZE)
    }

    pub fn decode_outcome_with_max_frame_size(
        input: &[u8],
        max_frame_size: usize,
    ) -> H2DecodeOutcome {
        match H2FrameRef::decode_outcome_with_max_frame_size(input, max_frame_size) {
            H2FrameRefDecodeOutcome::Frame { frame, consumed } => H2DecodeOutcome::Frame {
                frame: frame.to_owned(),
                consumed,
            },
            H2FrameRefDecodeOutcome::NeedMore => H2DecodeOutcome::NeedMore,
            H2FrameRefDecodeOutcome::Error(error) => H2DecodeOutcome::Error(error),
        }
    }
}

impl<'a> H2FrameRef<'a> {
    pub fn encode(self, output: &mut Vec<u8>) {
        H2Frame::encode_header(
            self.frame_type,
            self.flags,
            self.stream_id,
            self.payload.len(),
            output,
        );
        output.extend_from_slice(self.payload);
    }

    pub fn decode(input: &'a [u8]) -> Result<(Self, usize), ServerError> {
        Self::decode_outcome(input).into_result()
    }

    pub fn decode_with_max_frame_size(
        input: &'a [u8],
        max_frame_size: usize,
    ) -> Result<(Self, usize), ServerError> {
        Self::decode_outcome_with_max_frame_size(input, max_frame_size).into_result()
    }

    pub fn decode_outcome(input: &'a [u8]) -> H2FrameRefDecodeOutcome<'a> {
        Self::decode_outcome_with_max_frame_size(input, H2_MAX_MAX_FRAME_SIZE)
    }

    pub fn decode_outcome_with_max_frame_size(
        input: &'a [u8],
        max_frame_size: usize,
    ) -> H2FrameRefDecodeOutcome<'a> {
        let head = match H2Frame::decode_header(input) {
            Ok(head) => head,
            Err(ServerError::NeedMore) => return H2FrameRefDecodeOutcome::NeedMore,
            Err(error) => {
                return H2FrameRefDecodeOutcome::Error(h2_error_from_server_error(error, None));
            }
        };
        if head.payload_len > max_frame_size {
            return H2FrameRefDecodeOutcome::Error(H2ProtocolError::connection(
                H2ErrorCode::FrameSizeError,
                "HTTP/2 frame exceeds configured maximum frame size",
            ));
        }
        match Self::decode_with_head(input, head) {
            Ok((frame, consumed)) => H2FrameRefDecodeOutcome::Frame { frame, consumed },
            Err(ServerError::NeedMore) => H2FrameRefDecodeOutcome::NeedMore,
            Err(error) => {
                H2FrameRefDecodeOutcome::Error(h2_error_from_server_error(error, Some(head)))
            }
        }
    }

    pub fn to_owned(self) -> H2Frame {
        H2Frame {
            frame_type: self.frame_type,
            flags: self.flags,
            stream_id: self.stream_id,
            payload: self.payload.to_vec(),
        }
    }

    pub const fn head(self) -> H2FrameHead {
        H2FrameHead {
            frame_type: self.frame_type,
            flags: self.flags,
            stream_id: self.stream_id,
            payload_len: self.payload.len(),
        }
    }

    pub(crate) fn decode_with_head(
        input: &'a [u8],
        head: H2FrameHead,
    ) -> Result<(Self, usize), ServerError> {
        let total = 9usize
            .checked_add(head.payload_len)
            .ok_or(ServerError::InvalidFrame)?;
        if input.len() < total {
            return Err(ServerError::NeedMore);
        }
        Ok((
            Self {
                frame_type: head.frame_type,
                flags: head.flags,
                stream_id: head.stream_id,
                payload: &input[9..total],
            },
            total,
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum H2FrameRefDecodeOutcome<'a> {
    Frame {
        frame: H2FrameRef<'a>,
        consumed: usize,
    },
    NeedMore,
    Error(H2ProtocolError),
}

impl<'a> H2FrameRefDecodeOutcome<'a> {
    pub fn into_result(self) -> Result<(H2FrameRef<'a>, usize), ServerError> {
        match self {
            Self::Frame { frame, consumed } => Ok((frame, consumed)),
            Self::NeedMore => Err(ServerError::NeedMore),
            Self::Error(error) => Err(error.into()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum H2DecodeOutcome {
    Frame { frame: H2Frame, consumed: usize },
    NeedMore,
    Error(H2ProtocolError),
}

impl H2DecodeOutcome {
    pub fn into_result(self) -> Result<(H2Frame, usize), ServerError> {
        match self {
            Self::Frame { frame, consumed } => Ok((frame, consumed)),
            Self::NeedMore => Err(ServerError::NeedMore),
            Self::Error(error) => Err(error.into()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H2ErrorScope {
    Connection,
    Stream(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H2ErrorCode {
    NoError = 0,
    ProtocolError = 1,
    InternalError = 2,
    FlowControlError = 3,
    SettingsTimeout = 4,
    StreamClosed = 5,
    FrameSizeError = 6,
    RefusedStream = 7,
    Cancel = 8,
    CompressionError = 9,
    ConnectError = 10,
    EnhanceYourCalm = 11,
    InadequateSecurity = 12,
    Http11Required = 13,
}

impl H2ErrorCode {
    /// Returns the HTTP/2 wire value for this error code.
    pub const fn as_u32(self) -> u32 {
        self as u32
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2ProtocolError {
    pub scope: H2ErrorScope,
    pub code: H2ErrorCode,
    pub debug: &'static str,
    pub hpack_error: Option<H2HpackError>,
    /// Semantic resource-limit category, when this is a limit violation.
    pub http_error_kind: Option<crate::HttpErrorKind>,
    /// Configured and observed values for a resource-limit violation.
    pub limit: Option<crate::LimitViolation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
/// Stable, content-free categories for HPACK failures.
pub enum H2HpackError {
    /// An indexed representation refers outside the static and dynamic tables.
    HeaderIndexOutOfBounds,
    /// A prefixed integer is truncated or overflows.
    IntegerDecoding,
    /// A string length or payload is truncated or invalid.
    StringDecoding,
    /// A dynamic table size update exceeds the configured decoder maximum.
    InvalidMaxDynamicSize,
    /// A dynamic table size update has an invalid value, count, or position.
    InvalidTableSizeUpdate,
    /// A Huffman string contains EOS, invalid padding, or an invalid code.
    InvalidHuffman,
    /// A dynamic table size update appears after a field representation.
    TableSizeUpdateAfterField,
    /// The decoded field-section size exceeds the caller's limit.
    HeaderListTooLarge,
    /// The encoded header block exceeds the connection's configured limit.
    EncodedHeaderBlockTooLarge,
    /// Header field size accounting overflowed.
    FieldSizeOverflow,
    /// Internal HPACK state counters exhausted their representable range.
    StateOverflow,
    /// The decoder was already invalidated by a compression failure.
    DecoderPoisoned,
    /// A governed HPACK allocation could not be satisfied.
    AllocationFailed,
}

impl std::fmt::Display for H2HpackError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::HeaderIndexOutOfBounds => "HPACK header index is out of bounds",
            Self::IntegerDecoding => "HPACK integer is invalid",
            Self::StringDecoding => "HPACK string is invalid",
            Self::InvalidMaxDynamicSize => "HPACK dynamic table size update is invalid",
            Self::InvalidTableSizeUpdate => "HPACK dynamic table size update is invalid",
            Self::InvalidHuffman => "HPACK Huffman string is invalid",
            Self::TableSizeUpdateAfterField => {
                "HPACK dynamic table size update follows a header field"
            }
            Self::HeaderListTooLarge => "decoded HPACK header list exceeds its limit",
            Self::EncodedHeaderBlockTooLarge => "encoded HTTP/2 header block exceeds its limit",
            Self::FieldSizeOverflow => "HPACK field size overflows the platform size",
            Self::StateOverflow => "HPACK state exceeds its representable range",
            Self::DecoderPoisoned => "HPACK decoder is unavailable after a compression failure",
            Self::AllocationFailed => "HPACK storage allocation failed",
        })
    }
}

impl std::error::Error for H2HpackError {}

/// A content-free, directional snapshot of one connection's HPACK lifetime counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct H2HpackDiagnosticsSnapshot {
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
    pub local_limit_failures: u64,
    pub field_octets: u64,
    pub wire_octets: u64,
}

/// An exact reduced HPACK wire-to-field-octet ratio.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct H2HpackEffectiveness {
    pub encoded_wire_octets: u64,
    pub uncompressed_field_octets: u64,
}

impl H2HpackDiagnosticsSnapshot {
    /// Returns an exact reduced `wire / field` fraction when neither operand
    /// saturated and at least one uncompressed field octet was observed.
    pub fn effectiveness(self) -> Option<H2HpackEffectiveness> {
        if self.wire_octets == u64::MAX || self.field_octets == 0 || self.field_octets == u64::MAX {
            return None;
        }
        let divisor = greatest_common_divisor(self.wire_octets, self.field_octets);
        Some(H2HpackEffectiveness {
            encoded_wire_octets: self.wire_octets / divisor,
            uncompressed_field_octets: self.field_octets / divisor,
        })
    }
}

impl From<crate::hpack::Diagnostics> for H2HpackDiagnosticsSnapshot {
    fn from(value: crate::hpack::Diagnostics) -> Self {
        Self {
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
            local_limit_failures: value.header_list_too_large,
            field_octets: value.field_bytes,
            wire_octets: value.wire_bytes,
        }
    }
}

pub(crate) const fn greatest_common_divisor(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    if left == 0 { 1 } else { left }
}

impl From<crate::hpack::Error> for H2HpackError {
    fn from(error: crate::hpack::Error) -> Self {
        match error {
            crate::hpack::Error::InvalidIndex => Self::HeaderIndexOutOfBounds,
            crate::hpack::Error::IntegerOverflow | crate::hpack::Error::TruncatedInteger => {
                Self::IntegerDecoding
            }
            crate::hpack::Error::TruncatedString => Self::StringDecoding,
            crate::hpack::Error::InvalidMaxDynamicSize => Self::InvalidMaxDynamicSize,
            crate::hpack::Error::InvalidTableSizeUpdate => Self::InvalidTableSizeUpdate,
            crate::hpack::Error::TableSizeUpdateAfterField => Self::TableSizeUpdateAfterField,
            crate::hpack::Error::InvalidHuffman => Self::InvalidHuffman,
            crate::hpack::Error::HeaderListTooLarge { .. } => Self::HeaderListTooLarge,
            crate::hpack::Error::FieldSizeOverflow => Self::FieldSizeOverflow,
            crate::hpack::Error::StateOverflow => Self::StateOverflow,
            crate::hpack::Error::DecoderPoisoned => Self::DecoderPoisoned,
            crate::hpack::Error::AllocationFailed => Self::AllocationFailed,
        }
    }
}

impl From<H2ProtocolError> for ServerError {
    fn from(error: H2ProtocolError) -> Self {
        match error.code {
            H2ErrorCode::CompressionError => Self::InvalidHpack,
            H2ErrorCode::FrameSizeError
            | H2ErrorCode::ProtocolError
            | H2ErrorCode::FlowControlError
            | H2ErrorCode::StreamClosed
            | H2ErrorCode::RefusedStream
            | H2ErrorCode::Cancel
            | H2ErrorCode::SettingsTimeout
            | H2ErrorCode::EnhanceYourCalm
            | H2ErrorCode::NoError
            | H2ErrorCode::InternalError
            | H2ErrorCode::ConnectError
            | H2ErrorCode::InadequateSecurity
            | H2ErrorCode::Http11Required => Self::InvalidFrame,
        }
    }
}

impl H2ProtocolError {
    /// Returns whether this failure terminates the connection or one stream.
    pub const fn scope(&self) -> H2ErrorScope {
        self.scope
    }

    /// Returns the HTTP/2 error code that must be sent to the peer.
    pub const fn error_code(&self) -> H2ErrorCode {
        self.code
    }

    /// Returns stable, allocation-free semantic information about this failure.
    pub fn classify(&self) -> crate::HttpErrorInfo {
        use crate::{HttpErrorInfo, HttpErrorKind, HttpErrorScope};

        let kind = self.http_error_kind.unwrap_or(match self.code {
            H2ErrorCode::FlowControlError => HttpErrorKind::FlowControlViolation,
            H2ErrorCode::FrameSizeError => HttpErrorKind::InvalidFraming,
            H2ErrorCode::CompressionError => HttpErrorKind::Compression,
            H2ErrorCode::ProtocolError => HttpErrorKind::MalformedMessage,
            H2ErrorCode::StreamClosed
            | H2ErrorCode::InternalError
            | H2ErrorCode::SettingsTimeout => HttpErrorKind::InvalidState,
            H2ErrorCode::RefusedStream | H2ErrorCode::Cancel => HttpErrorKind::PeerReset,
            H2ErrorCode::NoError
            | H2ErrorCode::ConnectError
            | H2ErrorCode::EnhanceYourCalm
            | H2ErrorCode::InadequateSecurity
            | H2ErrorCode::Http11Required => HttpErrorKind::UnsupportedFeature,
        });
        let scope = match self.scope {
            H2ErrorScope::Connection => HttpErrorScope::Connection,
            H2ErrorScope::Stream(stream_id) => HttpErrorScope::Stream(stream_id),
        };
        HttpErrorInfo::new(kind, scope, self.debug, self.limit)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum H2FrameOutcome<E> {
    Event(E),
    Ignored,
    Error(H2ProtocolError),
}

impl<E> H2FrameOutcome<E> {
    pub(crate) fn map_event<T>(self, map: impl FnOnce(E) -> T) -> H2FrameOutcome<T> {
        match self {
            Self::Event(event) => H2FrameOutcome::Event(map(event)),
            Self::Ignored => H2FrameOutcome::Ignored,
            Self::Error(error) => H2FrameOutcome::Error(error),
        }
    }
}

impl H2ProtocolError {
    pub const fn connection(code: H2ErrorCode, debug: &'static str) -> Self {
        Self {
            scope: H2ErrorScope::Connection,
            code,
            debug,
            hpack_error: None,
            http_error_kind: None,
            limit: None,
        }
    }

    pub const fn stream(stream_id: u32, code: H2ErrorCode, debug: &'static str) -> Self {
        Self {
            scope: H2ErrorScope::Stream(stream_id),
            code,
            debug,
            hpack_error: None,
            http_error_kind: None,
            limit: None,
        }
    }

    pub const fn hpack(
        scope: H2ErrorScope,
        hpack_error: H2HpackError,
        debug: &'static str,
    ) -> Self {
        Self {
            scope,
            code: H2ErrorCode::CompressionError,
            debug,
            hpack_error: Some(hpack_error),
            http_error_kind: None,
            limit: None,
        }
    }

    pub(crate) const fn resource_limit(
        scope: H2ErrorScope,
        kind: crate::HttpErrorKind,
        limit: usize,
        actual: usize,
        debug: &'static str,
    ) -> Self {
        Self {
            scope,
            code: H2ErrorCode::EnhanceYourCalm,
            debug,
            hpack_error: None,
            http_error_kind: Some(kind),
            limit: Some(crate::LimitViolation::new(limit, Some(actual))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2Limits {
    pub max_frame_size: usize,
    pub max_settings_entries: usize,
    pub max_encoded_header_block_size: usize,
    pub max_header_list_size: usize,
    pub max_header_table_size: usize,
    pub max_continuation_frames: usize,
    pub max_active_streams: usize,
    pub max_queued_control_frames: usize,
    pub max_queued_data_bytes: usize,
    pub max_closed_stream_tombstones: usize,
}

impl Default for H2Limits {
    fn default() -> Self {
        Self {
            max_frame_size: H2Settings::default().max_frame_size,
            max_settings_entries: 64,
            max_encoded_header_block_size: H2_DEFAULT_MAX_HEADER_LIST_SIZE,
            max_header_list_size: H2_DEFAULT_MAX_HEADER_LIST_SIZE,
            max_header_table_size: H2Settings::default().header_table_size as usize,
            max_continuation_frames: 64,
            max_active_streams: H2_DEFAULT_MAX_ACTIVE_STREAMS,
            max_queued_control_frames: 10_000,
            max_queued_data_bytes: 16 * 1024 * 1024,
            max_closed_stream_tombstones: 1_024,
        }
    }
}

impl H2Limits {
    /// Derives HPACK, active-stream, and queued-DATA bounds from shared HTTP limits.
    pub fn from_http_limits(limits: HttpLimits) -> Self {
        Self {
            max_encoded_header_block_size: limits.max_header_bytes(),
            max_header_list_size: limits.max_header_bytes(),
            max_active_streams: limits.max_active_streams(),
            max_queued_data_bytes: limits.max_body_bytes(),
            ..Self::default()
        }
    }
}

impl From<HttpLimits> for H2Limits {
    fn from(limits: HttpLimits) -> Self {
        Self::from_http_limits(limits)
    }
}

pub(crate) fn validate_h2_limits(limits: H2Limits) -> Result<(), ServerError> {
    if limits.max_frame_size < H2_MIN_MAX_FRAME_SIZE
        || limits.max_frame_size > H2_MAX_MAX_FRAME_SIZE
        || limits.max_active_streams == 0
    {
        return Err(ServerError::InvalidFrame);
    }
    Ok(())
}

/// HTTP/2 SETTINGS identifiers understood by the FSM HTTP compatibility surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum H2SettingId {
    HeaderTableSize = 0x1,
    EnablePush = 0x2,
    MaxConcurrentStreams = 0x3,
    InitialWindowSize = 0x4,
    MaxFrameSize = 0x5,
    MaxHeaderListSize = 0x6,
}

impl H2SettingId {
    /// Converts a raw SETTINGS identifier into a known setting, ignoring unknown IDs.
    pub fn from_u16(value: u16) -> Option<Self> {
        match value {
            0x1 => Some(Self::HeaderTableSize),
            0x2 => Some(Self::EnablePush),
            0x3 => Some(Self::MaxConcurrentStreams),
            0x4 => Some(Self::InitialWindowSize),
            0x5 => Some(Self::MaxFrameSize),
            0x6 => Some(Self::MaxHeaderListSize),
            _ => None,
        }
    }
}

/// One decoded HTTP/2 SETTINGS entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2Setting {
    /// SETTINGS identifier.
    pub id: H2SettingId,
    /// Raw 32-bit SETTINGS value.
    pub value: u32,
}

impl H2Setting {
    /// Creates a SETTINGS entry from a known identifier and raw value.
    pub const fn new(id: H2SettingId, value: u32) -> Self {
        Self { id, value }
    }
}

/// Applied peer HTTP/2 settings relevant to frame/header adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2Settings {
    /// Header compression table size advertised by the peer.
    pub header_table_size: u32,
    /// Whether server push is enabled.
    pub enable_push: bool,
    /// Maximum concurrent streams advertised by the peer.
    pub max_concurrent_streams: u32,
    /// Initial stream flow-control window size.
    pub initial_window_size: u32,
    /// Maximum frame payload size.
    pub max_frame_size: usize,
    /// Maximum header list size advertised by the peer.
    pub max_header_list_size: u32,
}

impl Default for H2Settings {
    fn default() -> Self {
        Self {
            header_table_size: 4096,
            enable_push: true,
            max_concurrent_streams: u32::MAX,
            initial_window_size: 65_535,
            max_frame_size: 16 * 1024,
            max_header_list_size: u32::MAX,
        }
    }
}

impl H2Settings {
    /// Applies and validates one decoded SETTINGS entry.
    pub fn apply(&mut self, setting: H2Setting) -> Result<(), ServerError> {
        match setting.id {
            H2SettingId::HeaderTableSize => self.header_table_size = setting.value,
            H2SettingId::EnablePush => match setting.value {
                0 => self.enable_push = false,
                1 => self.enable_push = true,
                _ => return Err(ServerError::InvalidFrame),
            },
            H2SettingId::MaxConcurrentStreams => self.max_concurrent_streams = setting.value,
            H2SettingId::InitialWindowSize => {
                if setting.value > H2_MAX_WINDOW_SIZE {
                    return Err(ServerError::InvalidFrame);
                }
                self.initial_window_size = setting.value;
            }
            H2SettingId::MaxFrameSize => {
                self.max_frame_size = validate_max_frame_size(setting.value)?;
            }
            H2SettingId::MaxHeaderListSize => self.max_header_list_size = setting.value,
        }
        Ok(())
    }

    /// Applies a SETTINGS payload after it has been decoded into entries.
    pub fn apply_all(&mut self, settings: &[H2Setting]) -> Result<(), ServerError> {
        for &setting in settings {
            self.apply(setting)?;
        }
        Ok(())
    }

    /// Decodes a SETTINGS frame payload, ignoring unknown identifiers as required by HTTP/2.
    pub fn decode_payload(payload: &[u8]) -> Result<Vec<H2Setting>, ServerError> {
        Self::decode_payload_with_limit(payload, usize::MAX)
    }

    /// Decodes a SETTINGS payload and rejects payloads above a configured entry count.
    pub fn decode_payload_with_limit(
        payload: &[u8],
        max_entries: usize,
    ) -> Result<Vec<H2Setting>, ServerError> {
        if !payload.len().is_multiple_of(6) {
            return Err(ServerError::InvalidFrame);
        }
        let entries = payload.len() / 6;
        if entries > max_entries {
            return Err(ServerError::InvalidFrame);
        }
        payload
            .as_chunks::<6>()
            .0
            .iter()
            .filter_map(|setting| {
                let id = u16::from_be_bytes([setting[0], setting[1]]);
                H2SettingId::from_u16(id).map(|id| {
                    Ok(H2Setting::new(
                        id,
                        u32::from_be_bytes([setting[2], setting[3], setting[4], setting[5]]),
                    ))
                })
            })
            .collect()
    }

    /// Encodes SETTINGS entries into a frame payload.
    pub fn encode_payload(settings: &[H2Setting], output: &mut Vec<u8>) {
        for setting in settings {
            output.extend_from_slice(&(setting.id as u16).to_be_bytes());
            output.extend_from_slice(&setting.value.to_be_bytes());
        }
    }
}

pub(crate) fn window_update_increment(payload: &[u8]) -> Result<u32, ServerError> {
    if payload.len() != 4 {
        return Err(ServerError::InvalidFrame);
    }
    let mut increment = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    increment &= 0x7fff_ffff;
    if increment == 0 {
        return Err(ServerError::InvalidFrame);
    }
    Ok(increment)
}

pub(crate) fn data_payload(flags: u8, payload: &[u8]) -> Result<&[u8], ServerError> {
    strip_padding(flags, payload)
}

pub(crate) fn headers_payload(
    _stream_id: u32,
    flags: u8,
    payload: &[u8],
) -> Result<&[u8], ServerError> {
    let mut payload = strip_padding(flags, payload)?;
    if flags & 0x20 != 0 {
        if payload.len() < 5 {
            return Err(ServerError::InvalidFrame);
        }
        payload = &payload[5..];
    }
    Ok(payload)
}

pub(crate) fn priority_payload(stream_id: u32, payload: &[u8]) -> Result<(), ServerError> {
    if stream_id == 0 || payload.len() != 5 {
        return Err(ServerError::InvalidFrame);
    }
    validate_priority_dependency(stream_id, payload)
}

pub(crate) fn validate_priority_dependency(
    stream_id: u32,
    payload: &[u8],
) -> Result<(), ServerError> {
    let dependency =
        u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) & 0x7fff_ffff;
    if dependency == stream_id {
        return Err(ServerError::InvalidFrame);
    }
    Ok(())
}

pub(crate) fn strip_padding(flags: u8, payload: &[u8]) -> Result<&[u8], ServerError> {
    if flags & 0x8 == 0 {
        return Ok(payload);
    }
    let Some((&pad_len, rest)) = payload.split_first() else {
        return Err(ServerError::InvalidFrame);
    };
    let pad_len = pad_len as usize;
    if pad_len > rest.len() {
        return Err(ServerError::InvalidFrame);
    }
    Ok(&rest[..rest.len() - pad_len])
}

pub(crate) fn h2_error_from_server_error(
    error: ServerError,
    head: Option<H2FrameHead>,
) -> H2ProtocolError {
    let stream_id = head.map(|head| head.stream_id).unwrap_or(0);
    let frame_type = head.map(|head| head.frame_type);
    let connection_scope = matches!(
        error,
        ServerError::InvalidHpack
            | ServerError::UnsupportedHpack
            | ServerError::InvalidOutboundState
    ) || stream_id == 0
        || matches!(
            frame_type,
            Some(H2FrameType::Settings | H2FrameType::Ping | H2FrameType::Goaway)
        );
    let scope = if connection_scope {
        H2ErrorScope::Connection
    } else {
        H2ErrorScope::Stream(stream_id)
    };
    let code = match error {
        ServerError::InvalidHpack | ServerError::UnsupportedHpack => H2ErrorCode::CompressionError,
        ServerError::FlowControlViolation => H2ErrorCode::FlowControlError,
        ServerError::InvalidOutboundState => H2ErrorCode::InternalError,
        ServerError::InvalidFrame => {
            if matches!(
                head.map(|head| head.frame_type),
                Some(H2FrameType::WindowUpdate)
            ) {
                H2ErrorCode::FlowControlError
            } else {
                H2ErrorCode::ProtocolError
            }
        }
        ServerError::NeedMore => H2ErrorCode::ProtocolError,
        ServerError::MalformedMessage => H2ErrorCode::ProtocolError,
        ServerError::InvalidPreface
        | ServerError::UnsupportedAlpnProtocol
        | ServerError::Parse
        | ServerError::PeerReset { .. }
        | ServerError::PeerGoaway { .. }
        | ServerError::InvalidRequest
        | ServerError::InvalidResponse
        | ServerError::InvalidHeader
        | ServerError::InvalidContentLength
        | ServerError::TooManyHeaders { .. }
        | ServerError::BodyTooLarge { .. }
        | ServerError::HeaderTooLarge { .. }
        | ServerError::UnsupportedMethod
        | ServerError::UnsupportedVersion
        | ServerError::UnsupportedTransferEncoding => H2ErrorCode::ProtocolError,
    };
    let debug = match error {
        ServerError::NeedMore => "HTTP/2 frame needs more input",
        ServerError::InvalidHpack => "invalid HTTP/2 HPACK block",
        ServerError::UnsupportedHpack => "unsupported HTTP/2 HPACK representation",
        ServerError::InvalidPreface => "invalid HTTP/2 client preface",
        ServerError::UnsupportedAlpnProtocol => "unsupported TLS ALPN protocol",
        ServerError::InvalidFrame => "invalid HTTP/2 frame",
        ServerError::FlowControlViolation => "HTTP/2 flow-control operation was rejected",
        ServerError::InvalidOutboundState => "invalid HTTP/2 outbound state",
        _ => "invalid HTTP/2 protocol state",
    };
    H2ProtocolError {
        scope,
        code,
        debug,
        hpack_error: None,
        http_error_kind: match error {
            ServerError::HeaderTooLarge { .. } => Some(crate::HttpErrorKind::HeadersTooLarge),
            ServerError::TooManyHeaders { .. } => Some(crate::HttpErrorKind::TooManyHeaders),
            ServerError::BodyTooLarge { .. } => Some(crate::HttpErrorKind::BodyTooLarge),
            _ => None,
        },
        limit: match error {
            ServerError::HeaderTooLarge { limit, actual }
            | ServerError::TooManyHeaders { limit, actual }
            | ServerError::BodyTooLarge { limit, actual } => {
                Some(crate::LimitViolation::new(limit, Some(actual)))
            }
            _ => None,
        },
    }
}

pub(crate) fn validate_max_frame_size(value: u32) -> Result<usize, ServerError> {
    let value = value as usize;
    if !(H2_MIN_MAX_FRAME_SIZE..=H2_MAX_MAX_FRAME_SIZE).contains(&value) {
        return Err(ServerError::InvalidFrame);
    }
    Ok(value)
}

pub(crate) fn data_frames_encoded_len(payload_len: usize, max_frame_size: usize) -> usize {
    if payload_len == 0 {
        0
    } else {
        payload_len.saturating_add(payload_len.div_ceil(max_frame_size).saturating_mul(9))
    }
}

pub(crate) fn encode_data_frames(
    stream_id: u32,
    payload: &[u8],
    end_stream: bool,
    max_frame_size: usize,
    output: &mut Vec<u8>,
) {
    let mut remaining = payload;
    while !remaining.is_empty() {
        let frame_len = remaining.len().min(max_frame_size);
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
            output,
        );
        output.extend_from_slice(chunk);
    }
}
