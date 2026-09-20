//! HTTP/2 header views, HPACK wrappers, validation, and projection.

use core::fmt;

use crate::Header;
use crate::HttpLimits;
use crate::hpack_field_size;
use crate::parse_content_length;
use crate::server::ServerError;
use crate::server::h2::wire::{
    H2ErrorCode, H2ErrorScope, H2Frame, H2FrameType, H2HpackDiagnosticsSnapshot, H2HpackError,
    H2ProtocolError,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct H2Request {
    pub stream_id: u32,
    pub method: String,
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct H2Header {
    pub name: String,
    pub value: String,
    pub sensitive: bool,
}

impl H2Header {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            sensitive: false,
        }
    }

    pub const fn with_sensitive(mut self, sensitive: bool) -> Self {
        self.sensitive = sensitive;
        self
    }

    /// Borrows a regular text field for sensitivity-preserving forwarding.
    ///
    /// Pseudo-fields are interpreted into typed request/response roles during
    /// projection. They are therefore not exposed as the original occurrence.
    pub fn try_as_raw_occurrence(&self) -> Result<H2RawHeaderRef<'_>, H2HeaderProjectionError> {
        if self.name.starts_with(':') {
            return Err(H2HeaderProjectionError::PseudoHeaderNotForwardable);
        }
        Ok(
            H2RawHeaderRef::new(self.name.as_bytes(), self.value.as_bytes())
                .with_sensitive(self.sensitive),
        )
    }
}

/// Failure to project a canonical byte occurrence into the text convenience view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum H2HeaderProjectionError {
    NameNotUtf8,
    ValueNotUtf8,
    MalformedPseudoHeaders,
    PseudoHeaderNotForwardable,
}

impl fmt::Display for H2HeaderProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NameNotUtf8 => "HTTP/2 header name is not UTF-8",
            Self::ValueNotUtf8 => "HTTP/2 header value is not UTF-8",
            Self::MalformedPseudoHeaders => "HTTP/2 pseudo-header projection is invalid",
            Self::PseudoHeaderNotForwardable => {
                "typed HTTP/2 pseudo-header is not the original forwardable occurrence"
            }
        })
    }
}

impl std::error::Error for H2HeaderProjectionError {}

#[derive(Clone, Debug, Eq, PartialEq)]
/// The source-compatible owned HTTP/2 name/value pair.
pub struct H2RawHeader {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

impl H2RawHeader {
    pub fn new(name: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }

    /// Borrows this occurrence without copying its field bytes.
    pub fn as_ref(&self) -> H2RawHeaderRef<'_> {
        H2RawHeaderRef {
            name: &self.name,
            value: &self.value,
            sensitive: false,
        }
    }

    /// Converts this pair to a sensitivity-bearing field occurrence.
    pub fn with_sensitive(self, sensitive: bool) -> H2HeaderField {
        H2HeaderField {
            name: self.name,
            value: self.value,
            sensitive,
        }
    }
}

/// An owned occurrence for the fallible, sensitivity-preserving codec APIs.
pub use crate::hpack::HeaderField as H2HeaderField;

impl H2HeaderField {
    /// Borrows this occurrence without copying its field bytes.
    pub fn as_ref(&self) -> H2RawHeaderRef<'_> {
        H2RawHeaderRef {
            name: &self.name,
            value: &self.value,
            sensitive: self.sensitive,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// A borrowed, byte-preserving HTTP/2 header occurrence.
pub struct H2RawHeaderRef<'a> {
    /// Header or pseudo-header name bytes.
    pub name: &'a [u8],
    /// Header value bytes.
    pub value: &'a [u8],
    /// Whether this occurrence must use HPACK's never-indexed representation.
    pub sensitive: bool,
}

impl<'a> H2RawHeaderRef<'a> {
    /// Creates a nonsensitive borrowed header occurrence.
    pub const fn new(name: &'a [u8], value: &'a [u8]) -> Self {
        Self {
            name,
            value,
            sensitive: false,
        }
    }

    /// Sets whether this occurrence is sensitive.
    pub const fn with_sensitive(mut self, sensitive: bool) -> Self {
        self.sensitive = sensitive;
        self
    }

    /// Copies this occurrence into a sensitivity-bearing owned representation.
    pub fn to_owned(self) -> H2HeaderField {
        H2HeaderField {
            name: self.name.to_vec(),
            value: self.value.to_vec(),
            sensitive: self.sensitive,
        }
    }
}

#[derive(Clone, Copy)]
enum H2RawHeaderBlockSource<'a> {
    #[cfg(test)]
    Owned(&'a [H2HeaderField]),
    Compact {
        fields: &'a super::compact_headers::CompactHeaderFields,
        resolver: crate::hpack::IndexedHeaderFieldResolver<'a>,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct H2RawHeaderBlockRef<'a> {
    fields: H2RawHeaderBlockSource<'a>,
    start: usize,
    end: usize,
}

impl fmt::Debug for H2RawHeaderBlockRef<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_list()
            .entries((0..self.len()).filter_map(|index| self.get(index)))
            .finish()
    }
}

impl<'a> H2RawHeaderBlockRef<'a> {
    #[cfg(test)]
    pub(crate) fn new(fields: &'a [H2HeaderField]) -> Self {
        Self {
            fields: H2RawHeaderBlockSource::Owned(fields),
            start: 0,
            end: fields.len(),
        }
    }

    pub(crate) fn new_compact(
        fields: &'a super::compact_headers::CompactHeaderFields,
        resolver: crate::hpack::IndexedHeaderFieldResolver<'a>,
    ) -> Self {
        Self {
            fields: H2RawHeaderBlockSource::Compact { fields, resolver },
            start: 0,
            end: fields.len(),
        }
    }

    pub(crate) fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub(crate) fn get(self, index: usize) -> Option<H2RawHeaderRef<'a>> {
        if index >= self.len() {
            return None;
        }
        match self.fields {
            #[cfg(test)]
            H2RawHeaderBlockSource::Owned(fields) => {
                fields.get(self.start + index).map(H2HeaderField::as_ref)
            }
            H2RawHeaderBlockSource::Compact { fields, resolver } => {
                fields.get(self.start + index, resolver)
            }
        }
    }

    pub(crate) fn slice_from(self, start: usize) -> Option<Self> {
        if start > self.len() {
            return None;
        }
        Some(Self {
            fields: self.fields,
            start: self.start + start,
            end: self.end,
        })
    }

    pub(crate) fn iter(self) -> H2RawHeaderBlockIter<'a> {
        H2RawHeaderBlockIter {
            fields: self,
            index: 0,
        }
    }
}

pub(crate) struct H2RawHeaderBlockIter<'a> {
    fields: H2RawHeaderBlockRef<'a>,
    index: usize,
}

impl<'a> Iterator for H2RawHeaderBlockIter<'a> {
    type Item = H2RawHeaderRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let field = self.fields.get(self.index)?;
        self.index += 1;
        Some(field)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.fields.len().saturating_sub(self.index);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for H2RawHeaderBlockIter<'_> {}

/// A stateful HPACK encoder for one HTTP/2 direction.
///
/// State belongs to exactly one peer-bound direction and must be reused in
/// wire order:
///
/// ```
/// use kimojio_fsm_http2::{H2HeaderBlockEncoder, H2RawHeaderRef};
///
/// let mut encoder = H2HeaderBlockEncoder::new();
/// let secret =
///     H2RawHeaderRef::new(b"authorization", b"token").with_sensitive(true);
/// let block = encoder.try_encode_ref(&[secret])?;
/// assert_eq!(block[0] & 0xf0, 0x10);
/// # Ok::<(), kimojio_fsm_http2::H2HpackError>(())
/// ```
pub struct H2HeaderBlockEncoder {
    pub(crate) inner: Option<crate::hpack::Encoder>,
}

impl Clone for H2HeaderBlockEncoder {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.inner.clone_from(&source.inner);
    }
}

impl H2HeaderBlockEncoder {
    /// Creates an encoder with RFC 7541's 4,096-byte initial table capacity.
    pub const fn new() -> Self {
        Self { inner: None }
    }

    pub(crate) fn inner_mut(&mut self) -> &mut crate::hpack::Encoder {
        self.inner.get_or_insert_with(crate::hpack::Encoder::new)
    }

    /// Fallibly copies this encoder for a reversible pre-handoff transaction.
    ///
    /// The source encoder is unchanged if copying any retained history fails.
    #[doc(hidden)]
    pub fn try_clone_for_transaction(&self) -> Result<Self, H2HpackError> {
        let inner = self
            .inner
            .as_ref()
            .map(crate::hpack::Encoder::try_clone)
            .transpose()
            .map_err(H2HpackError::from)?;
        Ok(Self { inner })
    }

    /// Fallibly refreshes reusable transaction storage from another encoder.
    ///
    /// The source encoder is unchanged on failure.
    #[doc(hidden)]
    pub fn try_clone_from_for_transaction(&mut self, source: &Self) -> Result<(), H2HpackError> {
        match source.inner.as_ref() {
            Some(source) => self
                .inner_mut()
                .try_clone_from(source)
                .map_err(H2HpackError::from),
            None => {
                self.inner = None;
                Ok(())
            }
        }
    }

    /// Queues a dynamic table size update for the next encoded block.
    ///
    /// Multiple calls before [`Self::encode`] preserve the smallest requested
    /// size followed by the final size, as required by RFC 7541 section 4.2.
    /// Values above 1,048,576 octets are clamped to that local ceiling.
    pub fn set_max_table_size(&mut self, max_table_size: usize) {
        self.inner_mut().set_max_table_size(max_table_size);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_allocation_failure_after_for_testing(
        &mut self,
        successful_allocations: Option<usize>,
    ) {
        self.inner_mut()
            .set_allocation_failure_after(successful_allocations);
    }

    /// Encodes source-compatible nonsensitive pairs.
    ///
    /// Use [`Self::try_encode`] when allocation failure must be reported.
    pub fn encode(&mut self, headers: &[H2RawHeader]) -> Vec<u8> {
        self.try_encode(headers)
            .expect("HPACK allocation failed in compatibility encoder")
    }

    /// Fallibly encodes source-compatible nonsensitive pairs.
    pub fn try_encode(&mut self, headers: &[H2RawHeader]) -> Result<Vec<u8>, H2HpackError> {
        self.inner_mut()
            .encode_by(headers.len(), |index| {
                let header = &headers[index];
                crate::hpack::HeaderFieldRef {
                    name: &header.name,
                    value: &header.value,
                    sensitive: false,
                }
            })
            .map_err(Into::into)
    }

    /// Fallibly encodes owned sensitivity-bearing occurrences.
    pub fn try_encode_fields(
        &mut self,
        headers: &[H2HeaderField],
    ) -> Result<Vec<u8>, H2HpackError> {
        self.inner_mut()
            .encode_by(headers.len(), |index| {
                let header = &headers[index];
                crate::hpack::HeaderFieldRef {
                    name: &header.name,
                    value: &header.value,
                    sensitive: header.sensitive,
                }
            })
            .map_err(Into::into)
    }

    /// Fallibly encodes borrowed sensitivity-bearing occurrences.
    pub fn try_encode_ref(
        &mut self,
        headers: &[H2RawHeaderRef<'_>],
    ) -> Result<Vec<u8>, H2HpackError> {
        self.inner_mut()
            .encode_by(headers.len(), |index| {
                let header = headers[index];
                crate::hpack::HeaderFieldRef {
                    name: header.name,
                    value: header.value,
                    sensitive: header.sensitive,
                }
            })
            .map_err(Into::into)
    }

    /// Compatibility alias for [`Self::try_encode_ref`].
    pub fn encode_ref(&mut self, headers: &[H2RawHeaderRef<'_>]) -> Result<Vec<u8>, H2HpackError> {
        self.try_encode_ref(headers)
    }

    /// Returns a content-free snapshot for this outbound codec history.
    pub fn diagnostics(&mut self) -> H2HpackDiagnosticsSnapshot {
        self.inner_mut().diagnostics().into()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_diagnostics(&mut self) -> crate::hpack::Diagnostics {
        self.inner_mut().diagnostics()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_table_snapshot(&mut self) -> crate::hpack::TestTableSnapshot {
        self.inner_mut().test_table_snapshot()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_configured_max_size(&mut self) -> usize {
        self.inner_mut().test_configured_max_size()
    }
}

impl Default for H2HeaderBlockEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// A stateful HPACK decoder for one HTTP/2 direction.
///
/// A compression failure poisons the decoder. A decoded field-section limit
/// failure does not: compression state is committed and the next block remains
/// decodable.
///
/// ```
/// use kimojio_fsm_http2::{
///     H2HeaderBlockDecoder, H2HeaderBlockEncoder, H2RawHeader,
/// };
///
/// let field = H2RawHeader::new(b"x-bytes", [0, 0x80, 0xff]);
/// let block = H2HeaderBlockEncoder::new().encode(&[field.clone()]);
/// let decoded = H2HeaderBlockDecoder::new()
///     .decode_with_limit(&block, 1024)
///     .unwrap();
/// assert_eq!(decoded, [field]);
/// ```
pub struct H2HeaderBlockDecoder {
    pub(crate) inner: crate::hpack::Decoder,
}

impl H2HeaderBlockDecoder {
    /// Creates a decoder with RFC 7541's 4,096-byte initial table capacity.
    pub fn new() -> Self {
        Self {
            inner: crate::hpack::Decoder::new(),
        }
    }

    /// Sets the maximum dynamic table size accepted from subsequent blocks.
    ///
    /// Reducing the maximum requires the next block to begin with a size update
    /// no greater than the smallest reduction observed since the prior block.
    /// Values above 1,048,576 octets are clamped to that local ceiling.
    pub fn set_max_table_size(&mut self, max_table_size: usize) {
        self.inner.set_max_allowed_table_size(max_table_size);
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn set_allocation_failure_after_for_testing(
        &mut self,
        successful_allocations: Option<usize>,
    ) {
        self.inner
            .set_allocation_failure_after(successful_allocations);
    }

    /// Decodes one block through the source-compatible error and pair types.
    pub fn decode_with_limit(
        &mut self,
        bytes: &[u8],
        max_header_list_size: usize,
    ) -> Result<Vec<H2RawHeader>, ServerError> {
        let headers = self
            .try_decode_with_limit(bytes, max_header_list_size)
            .map_err(|_| ServerError::InvalidHpack)?;
        if headers.iter().any(|header| header.sensitive) {
            return Err(ServerError::InvalidHpack);
        }
        Ok(headers
            .into_iter()
            .map(|header| H2RawHeader::new(header.name, header.value))
            .collect())
    }

    /// Fallibly decodes one block with stable HPACK categories and sensitivity.
    pub fn try_decode_with_limit(
        &mut self,
        bytes: &[u8],
        max_header_list_size: usize,
    ) -> Result<Vec<H2HeaderField>, H2ProtocolError> {
        let headers = decode_hpack_with_limit(&mut self.inner, bytes, max_header_list_size)
            .map_err(|error| match error {
                H2HeaderDecodeError::Hpack(error) => {
                    let category = H2HpackError::from(error);
                    if category == H2HpackError::AllocationFailed {
                        H2ProtocolError {
                            scope: H2ErrorScope::Connection,
                            code: H2ErrorCode::InternalError,
                            debug: "HPACK storage allocation failed",
                            hpack_error: Some(category),
                            http_error_kind: None,
                            limit: None,
                        }
                    } else {
                        H2ProtocolError::hpack(
                            H2ErrorScope::Connection,
                            category,
                            "invalid HPACK block",
                        )
                    }
                }
                H2HeaderDecodeError::HeaderListTooLarge { actual } => {
                    let mut error = H2ProtocolError::resource_limit(
                        H2ErrorScope::Connection,
                        crate::HttpErrorKind::HeadersTooLarge,
                        max_header_list_size,
                        actual,
                        "decoded header list exceeds configured limit",
                    );
                    error.hpack_error = Some(H2HpackError::HeaderListTooLarge);
                    error
                }
            })?;
        Ok(headers)
    }

    /// Returns a content-free snapshot for this inbound codec history.
    pub fn diagnostics(&self) -> H2HpackDiagnosticsSnapshot {
        self.inner.diagnostics().into()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_diagnostics(&self) -> crate::hpack::Diagnostics {
        self.inner.diagnostics()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_table_snapshot(&self) -> crate::hpack::TestTableSnapshot {
        self.inner.test_table_snapshot()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_configured_max_size(&self) -> usize {
        self.inner.test_configured_max_size()
    }

    #[cfg(feature = "hpack-test-support")]
    pub(crate) fn test_post_limit_allocations(
        &self,
    ) -> Option<crate::hpack::TestPostLimitAllocationSnapshot> {
        self.inner.test_post_limit_allocations()
    }
}

pub(crate) enum H2HeaderDecodeError {
    Hpack(crate::hpack::Error),
    HeaderListTooLarge { actual: usize },
}

pub(crate) type H2DecodedHeaderList = Vec<crate::hpack::HeaderField>;

pub(crate) fn decode_hpack_with_limit(
    decoder: &mut crate::hpack::Decoder,
    bytes: &[u8],
    max_header_list_size: usize,
) -> Result<H2DecodedHeaderList, H2HeaderDecodeError> {
    match decoder.decode(bytes, max_header_list_size) {
        Ok(headers) => Ok(headers),
        Err(crate::hpack::Error::HeaderListTooLarge { actual }) => {
            Err(H2HeaderDecodeError::HeaderListTooLarge { actual })
        }
        Err(error) => Err(H2HeaderDecodeError::Hpack(error)),
    }
}

impl Default for H2HeaderBlockDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum H2HeaderValidationRole {
    Request,
    Response,
    Trailers,
}

#[cfg(test)]
pub(crate) fn h2_field_totals(
    headers: &[H2HeaderField],
    role: H2HeaderValidationRole,
) -> (usize, usize) {
    let regular_count = headers
        .iter()
        .filter(|header| !header.name.starts_with(b":"))
        .count();
    let synthesized_host = matches!(role, H2HeaderValidationRole::Request)
        && headers.iter().any(|header| header.name == b":authority")
        && !headers.iter().any(|header| header.name == b"host");
    let count = regular_count.saturating_add(usize::from(synthesized_host));
    let bytes = headers.iter().fold(0usize, |total, header| {
        total.saturating_add(hpack_field_size(&header.name, &header.value))
    });
    (count, bytes)
}

#[cfg(test)]
pub(crate) fn enforce_h2_field_limits(
    headers: &[H2HeaderField],
    role: H2HeaderValidationRole,
    limits: HttpLimits,
) -> Result<(), ServerError> {
    let (count, bytes) = h2_field_totals(headers, role);
    if count > limits.max_headers() {
        return Err(ServerError::TooManyHeaders {
            limit: limits.max_headers(),
            actual: count,
        });
    }
    if bytes > limits.max_header_bytes() {
        return Err(ServerError::HeaderTooLarge {
            limit: limits.max_header_bytes(),
            actual: bytes,
        });
    }
    Ok(())
}

/// Private facts from one successful HTTP/2 field-section validation.
///
/// Indexes refer to the caller-owned field slice. The summary never copies
/// header buffers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedSection {
    pub(crate) role: H2HeaderValidationRole,
    /// Regular-field count plus one synthesized `host` when a request has
    /// `:authority` and no `host` field.
    pub(crate) field_count: usize,
    /// Sum of RFC 7541 header-list sizes across every field in the section.
    pub(crate) field_bytes: usize,
    pub(crate) first_regular_index: usize,
    pub(crate) content_length: Option<usize>,
    pub(crate) request: Option<ValidatedRequestFacts>,
    pub(crate) response_status: Option<u16>,
}

#[cfg(test)]
pub(crate) struct ValidatedHeaderSection {
    section: ValidatedSection,
    fields_ptr: usize,
    fields_len: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ValidatedHeaderSectionRef<'a> {
    fields: H2RawHeaderBlockRef<'a>,
    section: ValidatedSection,
}

#[cfg(test)]
impl ValidatedHeaderSection {
    pub(crate) fn new(fields: &[H2HeaderField], section: ValidatedSection) -> Self {
        Self {
            section,
            fields_ptr: fields.as_ptr() as usize,
            fields_len: fields.len(),
        }
    }

    pub(crate) fn bind<'a>(
        self,
        fields: &'a [H2HeaderField],
        role: H2HeaderValidationRole,
    ) -> Result<ValidatedHeaderSectionRef<'a>, ServerError> {
        if self.fields_ptr != fields.as_ptr() as usize
            || self.fields_len != fields.len()
            || self.section.role != role
        {
            return Err(ServerError::InvalidFrame);
        }
        Ok(ValidatedHeaderSectionRef {
            fields: H2RawHeaderBlockRef::new(fields),
            section: self.section,
        })
    }
}

impl<'a> ValidatedHeaderSectionRef<'a> {
    pub(crate) fn new_compact(
        fields: &'a super::compact_headers::CompactHeaderFields,
        resolver: crate::hpack::IndexedHeaderFieldResolver<'a>,
        section: ValidatedSection,
    ) -> Self {
        Self {
            fields: H2RawHeaderBlockRef::new_compact(fields, resolver),
            section,
        }
    }

    pub(crate) fn into_parts(self) -> (H2RawHeaderBlockRef<'a>, ValidatedSection) {
        (self.fields, self.section)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedRequestFacts {
    pub(crate) method_index: usize,
    pub(crate) scheme_index: Option<usize>,
    pub(crate) authority_index: Option<usize>,
    pub(crate) path_index: usize,
    pub(crate) has_host: bool,
}

impl ValidatedSection {
    pub(crate) fn enforce_limits(self, limits: HttpLimits) -> Result<Self, ServerError> {
        if self.field_count > limits.max_headers() {
            return Err(ServerError::TooManyHeaders {
                limit: limits.max_headers(),
                actual: self.field_count,
            });
        }
        if self.field_bytes > limits.max_header_bytes() {
            return Err(ServerError::HeaderTooLarge {
                limit: limits.max_header_bytes(),
                actual: self.field_bytes,
            });
        }
        Ok(self)
    }

    pub(crate) fn enforce_h2_outbound_limits(
        self,
        stream_id: u32,
        limits: HttpLimits,
    ) -> Result<Self, H2ProtocolError> {
        let scope = H2ErrorScope::Stream(stream_id);
        if self.field_count > limits.max_headers() {
            return Err(H2ProtocolError::resource_limit(
                scope,
                crate::HttpErrorKind::TooManyHeaders,
                limits.max_headers(),
                self.field_count,
                "HTTP/2 header count exceeds configured limit",
            ));
        }
        if self.field_bytes > limits.max_header_bytes() {
            return Err(H2ProtocolError::resource_limit(
                scope,
                crate::HttpErrorKind::HeadersTooLarge,
                limits.max_header_bytes(),
                self.field_bytes,
                "HTTP/2 header list exceeds configured limit",
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum H2ValidatedName {
    Other,
    Method,
    Scheme,
    Authority,
    Path,
    Status,
    Te,
    Host,
    ContentLength,
    Forbidden,
    UnknownPseudo,
}

#[derive(Clone, Copy)]
pub(crate) enum H2ContentLengthState {
    Start,
    Digits,
    OwsAfterDigits,
    AfterComma,
}

const H2_CLASSIFIED_NAME_BYTES: usize = b"transfer-encoding".len();

pub(crate) struct H2HeaderValidator {
    pub(crate) role: H2HeaderValidationRole,
    pub(crate) invalid: bool,
    pub(crate) invalid_content_length: bool,
    pub(crate) saw_regular: bool,
    pub(crate) saw_method: bool,
    pub(crate) saw_scheme: bool,
    pub(crate) saw_authority: bool,
    pub(crate) saw_path: bool,
    pub(crate) saw_status: bool,
    pub(crate) content_length: Option<usize>,
    pub(crate) field_index: usize,
    pub(crate) regular_count: usize,
    pub(crate) field_bytes: usize,
    pub(crate) first_regular_index: Option<usize>,
    pub(crate) method_index: Option<usize>,
    pub(crate) scheme_index: Option<usize>,
    pub(crate) authority_index: Option<usize>,
    pub(crate) path_index: Option<usize>,
    pub(crate) has_host: bool,
    pub(crate) response_status: Option<u16>,
    pub(crate) name: [u8; H2_CLASSIFIED_NAME_BYTES],
    pub(crate) name_len: usize,
    pub(crate) name_buffered: bool,
    pub(crate) name_starts_with_colon: bool,
    pub(crate) current_name: H2ValidatedName,
    pub(crate) value_len: usize,
    pub(crate) value_ends_with_whitespace: bool,
    pub(crate) te_trailers: bool,
    pub(crate) status_value: Option<u16>,
    pub(crate) content_length_state: H2ContentLengthState,
    pub(crate) content_length_item: usize,
    pub(crate) content_length_field_value: Option<usize>,
}

impl H2HeaderValidator {
    pub(crate) fn new(role: H2HeaderValidationRole) -> Self {
        Self {
            role,
            invalid: false,
            invalid_content_length: false,
            saw_regular: false,
            saw_method: false,
            saw_scheme: false,
            saw_authority: false,
            saw_path: false,
            saw_status: false,
            content_length: None,
            field_index: 0,
            regular_count: 0,
            field_bytes: 0,
            first_regular_index: None,
            method_index: None,
            scheme_index: None,
            authority_index: None,
            path_index: None,
            has_host: false,
            response_status: None,
            name: [0; H2_CLASSIFIED_NAME_BYTES],
            name_len: 0,
            name_buffered: false,
            name_starts_with_colon: false,
            current_name: H2ValidatedName::Other,
            value_len: 0,
            value_ends_with_whitespace: false,
            te_trailers: true,
            status_value: Some(0),
            content_length_state: H2ContentLengthState::Start,
            content_length_item: 0,
            content_length_field_value: None,
        }
    }

    pub(crate) fn finish(mut self) -> Result<ValidatedSection, H2HeaderValidationError> {
        match self.role {
            H2HeaderValidationRole::Request => {
                self.invalid |= !self.saw_method || !self.saw_scheme || !self.saw_path;
            }
            H2HeaderValidationRole::Response => self.invalid |= !self.saw_status,
            H2HeaderValidationRole::Trailers => {}
        }
        if self.invalid_content_length {
            Err(H2HeaderValidationError::InvalidContentLength)
        } else if self.invalid {
            Err(H2HeaderValidationError::MalformedMessage)
        } else {
            let synthesized_host = matches!(self.role, H2HeaderValidationRole::Request)
                && self.authority_index.is_some()
                && !self.has_host;
            let request = match (self.role, self.method_index, self.path_index) {
                (H2HeaderValidationRole::Request, Some(method_index), Some(path_index)) => {
                    Some(ValidatedRequestFacts {
                        method_index,
                        scheme_index: self.scheme_index,
                        authority_index: self.authority_index,
                        path_index,
                        has_host: self.has_host,
                    })
                }
                _ => None,
            };
            Ok(ValidatedSection {
                role: self.role,
                field_count: self
                    .regular_count
                    .saturating_add(usize::from(synthesized_host)),
                field_bytes: self.field_bytes,
                first_regular_index: self.first_regular_index.unwrap_or(self.field_index),
                content_length: self.content_length,
                request,
                response_status: self.response_status,
            })
        }
    }

    /// Returns a byte-at-a-time field name only when it fit the inline buffer.
    ///
    /// `name_len` counts every byte the peer sent, not the bytes retained, so
    /// it can exceed the buffer. A name that overflows cannot match any name
    /// this validator recognises, and `get` reports that as `None` without
    /// indexing past the buffer.
    pub(crate) fn stored_name(&self) -> Option<&[u8]> {
        self.name.get(..self.name_len)
    }

    pub(crate) fn classify_name(name: &[u8]) -> H2ValidatedName {
        match name {
            b":method" => H2ValidatedName::Method,
            b":scheme" => H2ValidatedName::Scheme,
            b":authority" => H2ValidatedName::Authority,
            b":path" => H2ValidatedName::Path,
            b":status" => H2ValidatedName::Status,
            b"te" => H2ValidatedName::Te,
            b"host" => H2ValidatedName::Host,
            b"content-length" => H2ValidatedName::ContentLength,
            b"connection" | b"keep-alive" | b"proxy-connection" | b"transfer-encoding"
            | b"upgrade" => H2ValidatedName::Forbidden,
            _ if name.starts_with(b":") => H2ValidatedName::UnknownPseudo,
            _ => H2ValidatedName::Other,
        }
    }

    pub(crate) fn mark_pseudo(&mut self, name: H2ValidatedName) {
        if self.saw_regular {
            self.invalid = true;
        }
        let seen = match name {
            H2ValidatedName::Method if matches!(self.role, H2HeaderValidationRole::Request) => {
                &mut self.saw_method
            }
            H2ValidatedName::Scheme if matches!(self.role, H2HeaderValidationRole::Request) => {
                &mut self.saw_scheme
            }
            H2ValidatedName::Authority if matches!(self.role, H2HeaderValidationRole::Request) => {
                &mut self.saw_authority
            }
            H2ValidatedName::Path if matches!(self.role, H2HeaderValidationRole::Request) => {
                &mut self.saw_path
            }
            H2ValidatedName::Status if matches!(self.role, H2HeaderValidationRole::Response) => {
                &mut self.saw_status
            }
            _ => {
                self.invalid = true;
                return;
            }
        };
        if *seen {
            self.invalid = true;
        }
        *seen = true;
    }

    pub(crate) fn finish_content_length_item(&mut self) {
        if !matches!(
            self.content_length_state,
            H2ContentLengthState::Digits | H2ContentLengthState::OwsAfterDigits
        ) {
            self.invalid = true;
            self.invalid_content_length = true;
            return;
        }
        let parsed = self.content_length_item;
        if self
            .content_length_field_value
            .replace(parsed)
            .is_some_and(|existing| existing != parsed)
        {
            self.invalid = true;
            self.invalid_content_length = true;
        }
    }

    pub(crate) fn content_length_value_byte(&mut self, byte: u8) {
        match (self.content_length_state, byte) {
            (H2ContentLengthState::Start | H2ContentLengthState::AfterComma, b'0'..=b'9')
            | (H2ContentLengthState::Digits, b'0'..=b'9') => {
                let Some(parsed) = self
                    .content_length_item
                    .checked_mul(10)
                    .and_then(|number| number.checked_add(usize::from(byte - b'0')))
                else {
                    self.invalid = true;
                    self.invalid_content_length = true;
                    return;
                };
                self.content_length_item = parsed;
                self.content_length_state = H2ContentLengthState::Digits;
            }
            (H2ContentLengthState::Digits, b' ' | b'\t') => {
                self.content_length_state = H2ContentLengthState::OwsAfterDigits;
            }
            (
                H2ContentLengthState::AfterComma | H2ContentLengthState::OwsAfterDigits,
                b' ' | b'\t',
            ) => {}
            (H2ContentLengthState::Digits | H2ContentLengthState::OwsAfterDigits, b',') => {
                self.finish_content_length_item();
                self.content_length_item = 0;
                self.content_length_state = H2ContentLengthState::AfterComma;
            }
            _ => {
                self.invalid = true;
                self.invalid_content_length = true;
            }
        }
    }

    pub(crate) fn finish_content_length(&mut self) {
        self.finish_content_length_item();
        let Some(parsed) = self.content_length_field_value else {
            self.invalid = true;
            self.invalid_content_length = true;
            return;
        };
        if self
            .content_length
            .replace(parsed)
            .is_some_and(|existing| existing != parsed)
        {
            self.invalid = true;
            self.invalid_content_length = true;
        }
    }
}

impl crate::hpack::HeaderFieldVisitor for H2HeaderValidator {
    fn start_field(&mut self, _sensitive: bool) {
        self.name_len = 0;
        self.name_buffered = false;
        self.name_starts_with_colon = false;
        self.current_name = H2ValidatedName::Other;
        self.value_len = 0;
        self.value_ends_with_whitespace = false;
        self.te_trailers = true;
        self.status_value = Some(0);
        self.content_length_state = H2ContentLengthState::Start;
        self.content_length_item = 0;
        self.content_length_field_value = None;
    }

    fn name_byte(&mut self, byte: u8) {
        if self.name_len == 0 {
            self.name_starts_with_colon = byte == b':';
        }
        self.name_buffered = true;
        if self.name_len < self.name.len() {
            self.name[self.name_len] = byte;
        }
        if byte.is_ascii_uppercase()
            || (byte == b':' && self.name_len != 0)
            || (byte != b':' && !is_h2_field_name_byte(byte))
        {
            self.invalid = true;
        }
        self.name_len = self.name_len.saturating_add(1);
    }

    fn name_bytes(&mut self, bytes: &[u8]) {
        self.name_starts_with_colon = bytes.first() == Some(&b':');
        self.name_len = bytes.len();
        self.name_buffered = false;
        self.current_name = Self::classify_name(bytes);
        self.invalid |= bytes.iter().enumerate().any(|(index, &byte)| {
            byte.is_ascii_uppercase()
                || (byte == b':' && index != 0)
                || (byte != b':' && !is_h2_field_name_byte(byte))
        });
    }

    fn end_name(&mut self) {
        if self.name_len == 0 {
            self.invalid = true;
        }
        if self.name_buffered {
            self.current_name = self.stored_name().map_or_else(
                || {
                    if self.name_starts_with_colon {
                        H2ValidatedName::UnknownPseudo
                    } else {
                        H2ValidatedName::Other
                    }
                },
                Self::classify_name,
            );
        }
        if self.name_starts_with_colon {
            self.mark_pseudo(self.current_name);
        } else {
            self.saw_regular = true;
            if matches!(
                self.current_name,
                H2ValidatedName::Forbidden | H2ValidatedName::UnknownPseudo
            ) {
                self.invalid = true;
            }
        }
    }

    fn value_byte(&mut self, byte: u8) {
        if self.current_name == H2ValidatedName::ContentLength {
            self.content_length_value_byte(byte);
        }
        if self.current_name == H2ValidatedName::Te {
            self.te_trailers &= b"trailers".get(self.value_len) == Some(&byte);
        }
        if self.current_name == H2ValidatedName::Status {
            self.status_value = self.status_value.and_then(|status| {
                if self.value_len < 3 && byte.is_ascii_digit() {
                    Some(status * 10 + u16::from(byte - b'0'))
                } else {
                    None
                }
            });
        }
        let whitespace = matches!(byte, b' ' | b'\t');
        if matches!(byte, 0 | b'\r' | b'\n') || (self.value_len == 0 && whitespace) {
            self.invalid = true;
        }
        self.value_ends_with_whitespace = whitespace;
        self.value_len = self.value_len.saturating_add(1);
    }

    fn value_bytes(&mut self, bytes: &[u8]) {
        self.value_len = bytes.len();
        self.value_ends_with_whitespace = matches!(bytes.last(), Some(b' ' | b'\t'));
        if bytes.iter().any(|&byte| matches!(byte, 0 | b'\r' | b'\n'))
            || matches!(bytes.first(), Some(b' ' | b'\t'))
            || self.value_ends_with_whitespace
        {
            self.invalid = true;
        }
        if self.current_name == H2ValidatedName::ContentLength {
            for &byte in bytes {
                self.content_length_value_byte(byte);
            }
        }
        if self.current_name == H2ValidatedName::Te {
            self.te_trailers = bytes == b"trailers";
        }
        if self.current_name == H2ValidatedName::Status {
            self.status_value = match bytes {
                [
                    hundreds @ b'0'..=b'9',
                    tens @ b'0'..=b'9',
                    ones @ b'0'..=b'9',
                ] => Some(
                    u16::from(*hundreds - b'0') * 100
                        + u16::from(*tens - b'0') * 10
                        + u16::from(*ones - b'0'),
                ),
                _ => None,
            };
        }
    }

    fn end_field(&mut self) {
        if self.value_ends_with_whitespace {
            self.invalid = true;
        }
        match self.current_name {
            H2ValidatedName::Te => {
                if !self.te_trailers || self.value_len != b"trailers".len() {
                    self.invalid = true;
                }
            }
            H2ValidatedName::Status => {
                if let Some(status) = self
                    .status_value
                    .filter(|status| self.value_len == 3 && *status != 101)
                {
                    self.response_status = Some(status);
                } else {
                    self.invalid = true;
                }
            }
            H2ValidatedName::ContentLength => self.finish_content_length(),
            _ => {}
        }
        if self.name_starts_with_colon {
            match self.current_name {
                H2ValidatedName::Method => self.method_index = Some(self.field_index),
                H2ValidatedName::Scheme => self.scheme_index = Some(self.field_index),
                H2ValidatedName::Authority => self.authority_index = Some(self.field_index),
                H2ValidatedName::Path => self.path_index = Some(self.field_index),
                _ => {}
            }
        } else {
            if self.first_regular_index.is_none() {
                self.first_regular_index = Some(self.field_index);
            }
            self.regular_count = self.regular_count.saturating_add(1);
            self.has_host |= self.current_name == H2ValidatedName::Host;
        }
        self.field_bytes = self
            .field_bytes
            .saturating_add(hpack_field_size_from_lens(self.name_len, self.value_len));
        self.field_index = self.field_index.saturating_add(1);
    }
}

pub(crate) fn hpack_field_size_from_lens(name_len: usize, value_len: usize) -> usize {
    name_len
        .saturating_add(value_len)
        .saturating_add(crate::hpack_entry_overhead())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum H2HeaderValidationError {
    MalformedMessage,
    InvalidContentLength,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum H2PreparedRequestError {
    Request(ServerError),
    Protocol(H2ProtocolError),
}

impl From<H2ProtocolError> for H2PreparedRequestError {
    fn from(error: H2ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

impl From<H2PreparedRequestError> for ServerError {
    fn from(error: H2PreparedRequestError) -> Self {
        match error {
            H2PreparedRequestError::Request(error) => error,
            H2PreparedRequestError::Protocol(error) => error.into(),
        }
    }
}

impl From<H2HeaderValidationError> for ServerError {
    fn from(error: H2HeaderValidationError) -> Self {
        match error {
            H2HeaderValidationError::MalformedMessage => Self::MalformedMessage,
            H2HeaderValidationError::InvalidContentLength => Self::InvalidContentLength,
        }
    }
}

pub(crate) struct H2PreparedRequestValidator {
    validator: H2HeaderValidator,
    preparation_limits: HttpLimits,
    destination: Option<(u32, HttpLimits)>,
}

impl H2PreparedRequestValidator {
    pub(crate) fn new(
        preparation_limits: HttpLimits,
        destination_limits: HttpLimits,
        stream_id: u32,
    ) -> Self {
        Self {
            validator: H2HeaderValidator::new(H2HeaderValidationRole::Request),
            preparation_limits,
            destination: Some((stream_id, destination_limits)),
        }
    }

    pub(crate) fn for_preparation(preparation_limits: HttpLimits) -> Self {
        Self {
            validator: H2HeaderValidator::new(H2HeaderValidationRole::Request),
            preparation_limits,
            destination: None,
        }
    }
}

impl crate::hpack::EncodePreflightVisitor for H2PreparedRequestValidator {
    type Output = ();
    type Error = H2PreparedRequestError;
    const FINISH_AFTER_ENCODE_ERROR: bool = true;

    fn visit(&mut self, field: crate::hpack::HeaderFieldRef<'_>) {
        use crate::hpack::HeaderFieldVisitor;

        self.validator.start_field(field.sensitive);
        self.validator.name_bytes(field.name);
        self.validator.end_name();
        self.validator.value_bytes(field.value);
        self.validator.end_field();
    }

    fn finish(self) -> Result<Self::Output, Self::Error> {
        let section = self.validator.finish().map_err(|error| {
            H2PreparedRequestError::Request(match error {
                H2HeaderValidationError::MalformedMessage => ServerError::InvalidHeader,
                H2HeaderValidationError::InvalidContentLength => ServerError::InvalidContentLength,
            })
        })?;
        let section = section
            .enforce_limits(self.preparation_limits)
            .map_err(H2PreparedRequestError::Request)?;
        let Some((stream_id, destination_limits)) = self.destination else {
            return Ok(());
        };
        let section = section
            .enforce_h2_outbound_limits(stream_id, destination_limits)
            .map_err(H2PreparedRequestError::Protocol)?;
        let body_length = section.content_length.unwrap_or(0);
        if body_length > destination_limits.max_body_bytes() {
            return Err(H2PreparedRequestError::Protocol(h2_body_limit_error(
                stream_id,
                destination_limits,
                body_length,
            )));
        }
        Ok(())
    }
}

pub(crate) fn validate_prepared_request_by<'a>(
    stream_id: u32,
    field_count: usize,
    field_at: impl Fn(usize) -> H2RawHeaderRef<'a>,
    preparation_limits: HttpLimits,
    destination_limits: HttpLimits,
) -> Result<(), H2PreparedRequestError> {
    validate_prepared_request_with(
        field_count,
        field_at,
        H2PreparedRequestValidator::new(preparation_limits, destination_limits, stream_id),
    )
}

pub(crate) fn validate_prepared_request_for_preparation_by<'a>(
    field_count: usize,
    field_at: impl Fn(usize) -> H2RawHeaderRef<'a>,
    preparation_limits: HttpLimits,
) -> Result<(), H2PreparedRequestError> {
    validate_prepared_request_with(
        field_count,
        field_at,
        H2PreparedRequestValidator::for_preparation(preparation_limits),
    )
}

fn validate_prepared_request_with<'a>(
    field_count: usize,
    field_at: impl Fn(usize) -> H2RawHeaderRef<'a>,
    mut validator: H2PreparedRequestValidator,
) -> Result<(), H2PreparedRequestError> {
    use crate::hpack::EncodePreflightVisitor;

    for index in 0..field_count {
        let field = field_at(index);
        validator.visit(crate::hpack::HeaderFieldRef {
            name: field.name,
            value: field.value,
            sensitive: field.sensitive,
        });
    }
    validator.finish()
}

pub(crate) fn validate_decoded_header_fields(
    headers: &[H2HeaderField],
    role: H2HeaderValidationRole,
) -> Result<ValidatedSection, H2HeaderValidationError> {
    validate_decoded_header_fields_by(headers.len(), |index| headers[index].as_ref(), role)
}

pub(crate) fn validate_decoded_header_fields_by<'a>(
    field_count: usize,
    field_at: impl Fn(usize) -> H2RawHeaderRef<'a>,
    role: H2HeaderValidationRole,
) -> Result<ValidatedSection, H2HeaderValidationError> {
    use crate::hpack::HeaderFieldVisitor;

    let mut validator = H2HeaderValidator::new(role);
    for index in 0..field_count {
        let header = field_at(index);
        validator.start_field(header.sensitive);
        validator.name_bytes(header.name);
        validator.end_name();
        validator.value_bytes(header.value);
        validator.end_field();
    }
    validator.finish()
}

pub(crate) fn is_h2_field_name_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase()
        || byte.is_ascii_digit()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

pub(crate) fn project_h2_headers(
    headers: Vec<H2HeaderField>,
) -> Result<Vec<H2Header>, H2HeaderProjectionError> {
    project_h2_header_fields(&headers)
}

/// Typed HTTP/2 field-section role used by convenience projections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H2HeaderRole {
    /// Request pseudo-header rules.
    Request,
    /// Response pseudo-header rules.
    Response,
    /// Trailer rules, which prohibit pseudo-fields.
    Trailers,
}

/// Validates a typed role and atomically projects canonical fields to text.
pub fn project_h2_header_fields_for_role(
    headers: &[H2HeaderField],
    role: H2HeaderRole,
) -> Result<Vec<H2Header>, H2HeaderProjectionError> {
    let role = match role {
        H2HeaderRole::Request => H2HeaderValidationRole::Request,
        H2HeaderRole::Response => H2HeaderValidationRole::Response,
        H2HeaderRole::Trailers => H2HeaderValidationRole::Trailers,
    };
    validate_decoded_header_fields(headers, role)
        .map_err(|_| H2HeaderProjectionError::MalformedPseudoHeaders)?;
    project_h2_header_fields(headers)
}

/// Atomically projects canonical byte occurrences into the text convenience view.
///
/// The input remains unchanged when any name or value is not UTF-8.
pub fn project_h2_header_fields(
    headers: &[H2HeaderField],
) -> Result<Vec<H2Header>, H2HeaderProjectionError> {
    for header in headers {
        std::str::from_utf8(&header.name).map_err(|_| H2HeaderProjectionError::NameNotUtf8)?;
        std::str::from_utf8(&header.value).map_err(|_| H2HeaderProjectionError::ValueNotUtf8)?;
    }
    Ok(headers
        .iter()
        .map(|header| H2Header {
            name: String::from_utf8(header.name.clone()).expect("validated header name"),
            value: String::from_utf8(header.value.clone()).expect("validated header value"),
            sensitive: header.sensitive,
        })
        .collect())
}

#[cfg(test)]
pub(crate) fn status_from_raw_headers(headers: &[H2HeaderField]) -> Result<u16, ServerError> {
    let status = headers
        .iter()
        .find(|header| header.name == b":status")
        .ok_or(ServerError::MalformedMessage)?;
    if status.value.len() != 3 || !status.value.iter().all(u8::is_ascii_digit) {
        return Err(ServerError::MalformedMessage);
    }
    let hundreds = u16::from(status.value[0] - b'0') * 100;
    let tens = u16::from(status.value[1] - b'0') * 10;
    Ok(hundreds + tens + u16::from(status.value[2] - b'0'))
}

pub(crate) fn validate_regular_header(header: &H2Header) -> Result<(), ServerError> {
    if header.name.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(ServerError::MalformedMessage);
    }
    if matches!(
        header.name.as_str(),
        "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding" | "upgrade"
    ) {
        return Err(ServerError::MalformedMessage);
    }
    if header.name == "te" && header.value != "trailers" {
        return Err(ServerError::MalformedMessage);
    }
    Ok(())
}

pub(crate) fn status_from_headers(headers: &[H2Header]) -> Result<u16, ServerError> {
    let mut status = None;
    let mut saw_regular = false;
    for header in headers {
        if header.name.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(ServerError::MalformedMessage);
        }
        if header.name.starts_with(':') {
            if saw_regular || header.name != ":status" || status.is_some() {
                return Err(ServerError::MalformedMessage);
            }
            status = Some(header);
        } else {
            validate_regular_header(header)?;
            saw_regular = true;
        }
    }
    let status = status.ok_or(ServerError::MalformedMessage)?;
    if status.value.len() != 3 || !status.value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ServerError::MalformedMessage);
    }
    let status = status
        .value
        .parse::<u16>()
        .map_err(|_| ServerError::MalformedMessage)?;
    if status == 101 {
        return Err(ServerError::MalformedMessage);
    }
    Ok(status)
}

#[cfg(test)]
pub(crate) fn encode_hpack_request_headers(
    method: &str,
    scheme: &str,
    authority: &str,
    path: &str,
    headers: &[Header<'_>],
) -> Vec<u8> {
    let mut fields = Vec::with_capacity(headers.len() + 4);
    fields.push(H2RawHeader::new(":method", method));
    fields.push(H2RawHeader::new(":scheme", scheme));
    fields.push(H2RawHeader::new(":authority", authority));
    fields.push(H2RawHeader::new(":path", path));
    fields.extend(
        headers
            .iter()
            .map(|header| H2RawHeader::new(header.name, header.value)),
    );
    encode_hpack_raw_header_block(&fields)
}

#[cfg(test)]
pub(crate) fn encode_hpack_header_block(headers: &[Header<'_>]) -> Vec<u8> {
    let headers = headers
        .iter()
        .map(|header| H2RawHeader::new(header.name.as_bytes(), header.value.as_bytes()))
        .collect::<Vec<_>>();
    encode_hpack_raw_header_block(&headers)
}

#[cfg(test)]
pub(crate) fn encode_hpack_raw_header_block(headers: &[H2RawHeader]) -> Vec<u8> {
    H2HeaderBlockEncoder::new().encode(headers)
}

#[cfg(test)]
pub(crate) fn encode_hpack_response_headers(
    status: u16,
    content_length: usize,
    headers: &[Header<'_>],
) -> Vec<u8> {
    let mut fields = Vec::with_capacity(headers.len() + 2);
    let status = status.to_string();
    fields.push(H2RawHeader::new(":status", status));
    let content_length = content_length.to_string();
    fields.push(H2RawHeader::new("content-length", content_length));
    fields.extend(
        headers
            .iter()
            .map(|header| H2RawHeader::new(header.name, header.value)),
    );
    encode_hpack_raw_header_block(&fields)
}

#[cfg(test)]
pub(crate) fn hpack_push_string(out: &mut Vec<u8>, value: &[u8]) {
    hpack_push_prefixed_integer(out, value.len(), 0x7f, 0x00);
    out.extend_from_slice(value);
}

#[cfg(test)]
pub(crate) fn hpack_push_prefixed_integer(
    out: &mut Vec<u8>,
    value: usize,
    prefix_mask: u8,
    first_bits: u8,
) {
    let prefix_mask = prefix_mask as usize;
    if value < prefix_mask {
        out.push(first_bits | value as u8);
        return;
    }
    out.push(first_bits | prefix_mask as u8);
    let mut remaining = value - prefix_mask;
    while remaining >= 128 {
        out.push(((remaining & 0x7f) as u8) | 0x80);
        remaining >>= 7;
    }
    out.push(remaining as u8);
}

pub(crate) fn enforce_h2_outbound_field_limits<'a>(
    stream_id: u32,
    field_count: usize,
    field_at: impl Fn(usize) -> H2RawHeaderRef<'a>,
    limits: HttpLimits,
) -> Result<(), H2ProtocolError> {
    let mut count = 0usize;
    let mut bytes = 0usize;
    for index in 0..field_count {
        let field = field_at(index);
        if !field.name.starts_with(b":") {
            count = count.saturating_add(1);
        }
        bytes = bytes.saturating_add(hpack_field_size(field.name, field.value));
    }
    let scope = H2ErrorScope::Stream(stream_id);
    if count > limits.max_headers() {
        return Err(H2ProtocolError::resource_limit(
            scope,
            crate::HttpErrorKind::TooManyHeaders,
            limits.max_headers(),
            count,
            "HTTP/2 header count exceeds configured limit",
        ));
    }
    if bytes > limits.max_header_bytes() {
        return Err(H2ProtocolError::resource_limit(
            scope,
            crate::HttpErrorKind::HeadersTooLarge,
            limits.max_header_bytes(),
            bytes,
            "HTTP/2 header list exceeds configured limit",
        ));
    }
    Ok(())
}

pub(crate) fn h2_body_limit_error(
    stream_id: u32,
    limits: HttpLimits,
    actual: usize,
) -> H2ProtocolError {
    H2ProtocolError::resource_limit(
        H2ErrorScope::Stream(stream_id),
        crate::HttpErrorKind::BodyTooLarge,
        limits.max_body_bytes(),
        actual,
        "HTTP/2 body exceeds configured limit",
    )
}

pub(crate) fn enforce_optional_body_size(
    actual: usize,
    limit: Option<usize>,
) -> Result<(), ServerError> {
    if let Some(limit) = limit
        && actual > limit
    {
        Err(ServerError::BodyTooLarge { limit, actual })
    } else {
        Ok(())
    }
}

pub(crate) fn decoded_header_limit_error(
    scope: H2ErrorScope,
    limit: usize,
    actual: usize,
) -> H2ProtocolError {
    let mut error = H2ProtocolError::resource_limit(
        scope,
        crate::HttpErrorKind::HeadersTooLarge,
        limit,
        actual,
        "decoded header list exceeds configured limit",
    );
    error.hpack_error = Some(H2HpackError::HeaderListTooLarge);
    error
}

pub(crate) fn encoded_header_limit_error(limit: usize, actual: usize) -> H2ProtocolError {
    let mut error = H2ProtocolError::resource_limit(
        H2ErrorScope::Connection,
        crate::HttpErrorKind::HeadersTooLarge,
        limit,
        actual,
        "encoded header block exceeds configured limit",
    );
    error.hpack_error = Some(H2HpackError::EncodedHeaderBlockTooLarge);
    error
}

pub(crate) fn allocation_terminal_error() -> H2ProtocolError {
    H2ProtocolError {
        scope: H2ErrorScope::Connection,
        code: H2ErrorCode::InternalError,
        debug: "HPACK storage allocation failed",
        hpack_error: Some(H2HpackError::AllocationFailed),
        http_error_kind: None,
        limit: None,
    }
}

pub(crate) fn poisoned_connection_error() -> H2ProtocolError {
    H2ProtocolError::hpack(
        H2ErrorScope::Connection,
        H2HpackError::DecoderPoisoned,
        "HPACK decoder is unavailable after a compression failure",
    )
}

pub(crate) fn outbound_hpack_error(category: H2HpackError) -> H2ProtocolError {
    H2ProtocolError {
        scope: H2ErrorScope::Connection,
        code: H2ErrorCode::InternalError,
        debug: "outbound HPACK block could not be handed off",
        hpack_error: Some(category),
        http_error_kind: None,
        limit: None,
    }
}

pub(crate) struct H2ConnectionHpackErrorState<'a> {
    pub(crate) last_hpack_error: &'a mut Option<H2HpackError>,
    pub(crate) last_protocol_error: &'a mut Option<H2ProtocolError>,
    pub(crate) terminal_protocol_error: &'a mut Option<H2ProtocolError>,
}

pub(crate) fn decode_connection_header_fields(
    decoder: &mut crate::hpack::Decoder,
    headers: &mut impl crate::hpack::HeaderDecodeOutput,
    block: &[u8],
    max_header_list_size: usize,
    stream_id: u32,
    role: H2HeaderValidationRole,
    errors: H2ConnectionHpackErrorState<'_>,
) -> Result<ValidatedSection, ServerError> {
    let mut validator = H2HeaderValidator::new(role);
    let decoded =
        decoder.decode_with_visitor_into(block, max_header_list_size, &mut validator, headers);
    let validation = validator.finish();
    match decoded {
        Ok(()) => match validation {
            Ok(section) => Ok(section),
            Err(validation) => {
                *errors.last_protocol_error = Some(H2ProtocolError::stream(
                    stream_id,
                    H2ErrorCode::ProtocolError,
                    "HTTP/2 header field validation failed",
                ));
                Err(validation.into())
            }
        },
        Err(crate::hpack::Error::HeaderListTooLarge { actual }) => {
            if validation.is_err() {
                *errors.last_protocol_error = Some(H2ProtocolError::stream(
                    stream_id,
                    H2ErrorCode::ProtocolError,
                    "HTTP/2 header field validation failed",
                ));
            } else {
                *errors.last_protocol_error = Some(decoded_header_limit_error(
                    H2ErrorScope::Stream(stream_id),
                    max_header_list_size,
                    actual,
                ));
            }
            Err(ServerError::HeaderTooLarge {
                limit: max_header_list_size,
                actual,
            })
        }
        Err(crate::hpack::Error::AllocationFailed) => {
            let error = allocation_terminal_error();
            *errors.last_protocol_error = Some(error);
            *errors.terminal_protocol_error = Some(error);
            Err(ServerError::InvalidFrame)
        }
        Err(error) => {
            *errors.last_hpack_error = Some(error.into());
            *errors.terminal_protocol_error = Some(poisoned_connection_error());
            Err(ServerError::InvalidHpack)
        }
    }
}

pub(crate) fn request_from_headers(
    stream_id: u32,
    headers: &[H2Header],
) -> Result<H2Request, ServerError> {
    let mut method = None;
    let mut path = None;
    let mut scheme = None;
    let mut authority = None;
    let mut saw_regular = false;
    for header in headers {
        if header.name.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(ServerError::MalformedMessage);
        }
        if matches!(
            header.name.as_str(),
            "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding" | "upgrade"
        ) {
            return Err(ServerError::MalformedMessage);
        }
        if header.name.starts_with(':') {
            if saw_regular {
                return Err(ServerError::MalformedMessage);
            }
        } else {
            saw_regular = true;
        }
        match header.name.as_str() {
            ":method" if method.is_none() => method = Some(header.value.clone()),
            ":path" if path.is_none() => path = Some(header.value.clone()),
            ":scheme" if scheme.is_none() => scheme = Some(header.value.clone()),
            ":authority" if authority.is_none() => authority = Some(header.value.clone()),
            ":authority" => return Err(ServerError::MalformedMessage),
            name if name.starts_with(':') => return Err(ServerError::MalformedMessage),
            "te" if header.value != "trailers" => return Err(ServerError::MalformedMessage),
            "te" => {}
            _ => {}
        }
    }
    let _scheme = scheme.ok_or(ServerError::MalformedMessage)?;
    Ok(H2Request {
        stream_id,
        method: method.ok_or(ServerError::MalformedMessage)?,
        path: path.ok_or(ServerError::MalformedMessage)?,
    })
}

pub(crate) fn str_header_as_raw<'a>(header: &'a Header<'a>) -> H2RawHeaderRef<'a> {
    H2RawHeaderRef::new(header.name.as_bytes(), header.value.as_bytes())
}

pub(crate) fn content_length_from_raw_headers(
    headers: &[H2HeaderField],
) -> Result<Option<usize>, ServerError> {
    content_length_from_fields_by(headers.len(), |index| headers[index].as_ref())
}

pub(crate) fn content_length_from_fields_by<'a>(
    field_count: usize,
    field_at: impl Fn(usize) -> H2RawHeaderRef<'a>,
) -> Result<Option<usize>, ServerError> {
    let mut content_length = None;
    for index in 0..field_count {
        let header = field_at(index);
        if header.name == b"content-length" {
            let parsed =
                parse_content_length(header.value).ok_or(ServerError::InvalidContentLength)?;
            if content_length
                .replace(parsed)
                .is_some_and(|existing| existing != parsed)
            {
                return Err(ServerError::InvalidContentLength);
            }
        }
    }
    Ok(content_length)
}

pub(crate) fn compatibility_error(
    original: Option<ServerError>,
    protocol: H2ProtocolError,
) -> ServerError {
    match original {
        Some(
            error @ (ServerError::InvalidContentLength
            | ServerError::HeaderTooLarge { .. }
            | ServerError::TooManyHeaders { .. }
            | ServerError::BodyTooLarge { .. }),
        ) => error,
        _ => protocol.into(),
    }
}

pub(crate) fn recoverable_h2_message_error(error: &ServerError) -> bool {
    matches!(
        error,
        ServerError::Parse
            | ServerError::InvalidRequest
            | ServerError::InvalidResponse
            | ServerError::InvalidHeader
            | ServerError::InvalidContentLength
            | ServerError::TooManyHeaders { .. }
            | ServerError::BodyTooLarge { .. }
            | ServerError::HeaderTooLarge { .. }
            | ServerError::UnsupportedMethod
            | ServerError::UnsupportedVersion
            | ServerError::UnsupportedTransferEncoding
            | ServerError::FlowControlViolation
            | ServerError::MalformedMessage
    )
}

pub(crate) fn encode_connection_header_frames_by<'a>(
    encoder: &mut crate::hpack::Encoder,
    stream_id: u32,
    field_count: usize,
    field_at: impl Fn(usize) -> H2RawHeaderRef<'a> + Copy,
    end_stream: bool,
    max_frame_size: usize,
    reserved_tail: usize,
) -> Result<Vec<u8>, H2HpackError> {
    let mut output = FramedHpackOutput::new(stream_id, end_stream, max_frame_size, reserved_tail);
    encoder
        .encode_by_into(
            field_count,
            |index| {
                let field = field_at(index);
                crate::hpack::HeaderFieldRef {
                    name: field.name,
                    value: field.value,
                    sensitive: field.sensitive,
                }
            },
            &mut output,
        )
        .map_err(H2HpackError::from)?;
    Ok(output.finish())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_prepared_request_header_frames_by<'a>(
    encoder: &mut crate::hpack::Encoder,
    stream_id: u32,
    field_count: usize,
    field_at: impl Fn(usize) -> H2RawHeaderRef<'a> + Copy,
    end_stream: bool,
    max_frame_size: usize,
    preparation_limits: HttpLimits,
    destination_limits: HttpLimits,
) -> Result<Vec<u8>, H2PreparedRequestError> {
    let mut output = FramedHpackOutput::new(stream_id, end_stream, max_frame_size, 0);
    let validator =
        H2PreparedRequestValidator::new(preparation_limits, destination_limits, stream_id);
    encoder
        .encode_by_into_with_visitor(
            field_count,
            |index| {
                let field = field_at(index);
                crate::hpack::HeaderFieldRef {
                    name: field.name,
                    value: field.value,
                    sensitive: field.sensitive,
                }
            },
            validator,
            &mut output,
        )
        .map_err(|error| match error {
            crate::hpack::EncodePreflightError::Encode(error) => {
                H2PreparedRequestError::Protocol(outbound_hpack_error(error.into()))
            }
            crate::hpack::EncodePreflightError::Visitor(error) => error,
        })?;
    Ok(output.finish())
}

struct FramedHpackOutput {
    output: Vec<u8>,
    stream_id: u32,
    end_stream: bool,
    max_frame_size: usize,
    reserved_tail: usize,
    maximum_encoded_len: usize,
    payload_written: usize,
    current_frame_start: Option<usize>,
    current_frame_payload: usize,
}

impl FramedHpackOutput {
    fn new(stream_id: u32, end_stream: bool, max_frame_size: usize, reserved_tail: usize) -> Self {
        Self {
            output: Vec::new(),
            stream_id,
            end_stream,
            max_frame_size,
            reserved_tail,
            maximum_encoded_len: 0,
            payload_written: 0,
            current_frame_start: None,
            current_frame_payload: 0,
        }
    }

    fn begin_frame(&mut self) {
        if let Some(frame_start) = self.current_frame_start {
            self.write_current_frame_length(frame_start);
        }
        let first = self.payload_written == 0;
        let frame_start = self.output.len();
        H2Frame::encode_header(
            if first {
                H2FrameType::Headers
            } else {
                H2FrameType::Continuation
            },
            if first && self.end_stream { 0x1 } else { 0 },
            self.stream_id,
            0,
            &mut self.output,
        );
        self.current_frame_start = Some(frame_start);
        self.current_frame_payload = 0;
    }

    fn write_current_frame_length(&mut self, frame_start: usize) {
        let length = self.current_frame_payload;
        self.output[frame_start] = ((length >> 16) & 0xff) as u8;
        self.output[frame_start + 1] = ((length >> 8) & 0xff) as u8;
        self.output[frame_start + 2] = (length & 0xff) as u8;
    }

    fn finish(mut self) -> Vec<u8> {
        if self.current_frame_start.is_none() {
            self.begin_frame();
        }
        let frame_start = self.current_frame_start.expect("a final frame exists");
        self.write_current_frame_length(frame_start);
        self.output[frame_start + 4] |= 0x4;
        debug_assert!(self.payload_written <= self.maximum_encoded_len);
        self.output
    }
}

impl crate::hpack::EncodeOutput for FramedHpackOutput {
    fn try_reserve_exact(&mut self, maximum_encoded_len: usize) -> Result<(), crate::hpack::Error> {
        self.maximum_encoded_len = maximum_encoded_len;
        let frame_count = maximum_encoded_len.max(1).div_ceil(self.max_frame_size);
        let output_len = maximum_encoded_len
            .checked_add(
                frame_count
                    .checked_mul(9)
                    .ok_or(crate::hpack::Error::AllocationFailed)?,
            )
            .and_then(|length| length.checked_add(self.reserved_tail))
            .ok_or(crate::hpack::Error::AllocationFailed)?;
        self.output
            .try_reserve_exact(output_len)
            .map_err(|_| crate::hpack::Error::AllocationFailed)?;
        Ok(())
    }

    fn push(&mut self, byte: u8) {
        if self.current_frame_start.is_none() || self.current_frame_payload == self.max_frame_size {
            self.begin_frame();
        }
        self.output.push(byte);
        self.payload_written += 1;
        self.current_frame_payload += 1;
    }

    fn extend_from_slice(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.current_frame_start.is_none()
                || self.current_frame_payload == self.max_frame_size
            {
                self.begin_frame();
            }
            let available = self.max_frame_size - self.current_frame_payload;
            let chunk_len = available.min(bytes.len());
            let (chunk, remaining) = bytes.split_at(chunk_len);
            self.output.extend_from_slice(chunk);
            self.payload_written += chunk_len;
            self.current_frame_payload += chunk_len;
            bytes = remaining;
        }
    }

    fn encoded_len(&self) -> usize {
        self.payload_written
    }
}
