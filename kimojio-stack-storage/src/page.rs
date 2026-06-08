// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Page-object helpers for aligned random I/O.
//!
//! Page writes and clears require 512-byte alignment. Ambiguous write failures
//! are not directly retried by [`RetryPolicy`](crate::RetryPolicy); use
//! [`classify_page_write_failure`] or [`classify_sequence_number_failure`] to
//! decide whether to refresh properties before attempting recovery.
//!
//! ```
//! use kimojio_stack_storage::{PageRange, PAGE_ALIGNMENT};
//!
//! let range = PageRange::aligned_len(0, PAGE_ALIGNMENT).unwrap();
//! assert_eq!(range.header_value(), "bytes=0-511");
//! ```

use bytes::Bytes;

use crate::{
    AttemptError, Diagnostics, Error, ErrorKind, LeaseContext, MetadataMap, ObjectProperties,
    ObjectRef, OperationClass, ReplayBody, RequestParts, ResponseParts, Transport,
    ownership::apply_lease,
};

/// Required page-object range and length alignment in bytes.
pub const PAGE_ALIGNMENT: u64 = 512;

/// Inclusive byte range for page operations.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PageRange {
    start: u64,
    end: u64,
}

/// Optional conditions for page writes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PageWriteConditions {
    /// Require the object sequence number to equal this value.
    pub sequence_number_eq: Option<u64>,
}

impl PageWriteConditions {
    /// Adds an exact sequence-number precondition.
    pub fn with_sequence_number_eq(mut self, sequence_number: u64) -> Self {
        self.sequence_number_eq = Some(sequence_number);
        self
    }
}

impl PageRange {
    /// Creates an inclusive byte range without enforcing page alignment.
    pub fn new(start: u64, end: u64) -> Result<Self, Error> {
        if end < start {
            return Err(Error::new(
                ErrorKind::Range,
                "page range end precedes start",
            ));
        }
        Ok(Self { start, end })
    }

    /// Creates an aligned inclusive range from `start` and byte `len`.
    pub fn aligned_len(start: u64, len: u64) -> Result<Self, Error> {
        if len == 0 {
            return Err(Error::new(ErrorKind::Range, "page range length is zero"));
        }
        if !start.is_multiple_of(PAGE_ALIGNMENT) || !len.is_multiple_of(PAGE_ALIGNMENT) {
            return Err(Error::new(
                ErrorKind::Range,
                "page range start and length must be 512-byte aligned",
            ));
        }
        let end = start
            .checked_add(len - 1)
            .ok_or_else(|| Error::new(ErrorKind::Range, "page range overflows u64"))?;
        Self::new(start, end)
    }

    /// Returns the inclusive start offset.
    pub fn start(self) -> u64 {
        self.start
    }

    /// Returns the inclusive end offset.
    pub fn end(self) -> u64 {
        self.end
    }

    /// Returns the saturating inclusive length.
    pub fn len(self) -> u64 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }

    /// Returns the checked inclusive length.
    pub fn checked_len(self) -> Result<u64, Error> {
        self.end
            .checked_sub(self.start)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| Error::new(ErrorKind::Range, "page range length overflows u64"))
    }

    /// Page ranges are never empty because `end >= start`.
    pub fn is_empty(self) -> bool {
        false
    }

    /// Formats the range as an HTTP `bytes=start-end` header value.
    pub fn header_value(self) -> String {
        format!("bytes={}-{}", self.start, self.end)
    }
}

/// Sequence-number update action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SequenceNumberAction {
    /// Set the service sequence number to the larger of current and supplied values.
    Max,
    /// Replace the service sequence number.
    Update(u64),
    /// Increment the service sequence number.
    Increment,
}

/// Recommended recovery path after sequence-number operations fail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SequenceNumberRecovery {
    /// Refresh properties and reevaluate the caller's expected sequence number.
    RefreshProperties,
    /// Failure is terminal for the supplied error kind.
    Terminal(ErrorKind),
}

/// Recommended recovery path after page writes fail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageWriteRecovery {
    /// Refresh properties before deciding whether another write is safe.
    RefreshPropertiesBeforeRetry,
    /// Failure is terminal for the supplied error kind.
    Terminal(ErrorKind),
}

impl SequenceNumberAction {
    fn action_header(self) -> &'static str {
        match self {
            Self::Max => "max",
            Self::Update(_) => "update",
            Self::Increment => "increment",
        }
    }
}

/// Classifies a failed sequence-number operation for recovery handling.
pub fn classify_sequence_number_failure(error: &Error) -> SequenceNumberRecovery {
    if error.kind() == ErrorKind::SequenceNumber {
        SequenceNumberRecovery::RefreshProperties
    } else {
        SequenceNumberRecovery::Terminal(error.kind())
    }
}

/// Classifies a failed page write for recovery handling.
pub fn classify_page_write_failure(error: &Error) -> PageWriteRecovery {
    match error.kind() {
        ErrorKind::SequenceNumber
        | ErrorKind::Timeout
        | ErrorKind::Transport
        | ErrorKind::Unavailable => PageWriteRecovery::RefreshPropertiesBeforeRetry,
        kind => PageWriteRecovery::Terminal(kind),
    }
}

/// Client helper for page-object operations.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PageClient;

impl PageClient {
    /// Creates a page object with aligned total length.
    pub fn create<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        object: &ObjectRef,
        content_len: u64,
        metadata: &MetadataMap,
        lease: Option<&LeaseContext>,
    ) -> Result<ResponseParts, AttemptError> {
        let request = create_page_request(object, content_len, metadata, lease)
            .map_err(|error| attempt_error(OperationClass::PageWrite, error))?;
        transport.execute(cx, &request)
    }

    /// Uploads one aligned page range.
    pub fn upload_range<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        object: &ObjectRef,
        range: PageRange,
        body: ReplayBody,
        lease: Option<&LeaseContext>,
    ) -> Result<ResponseParts, AttemptError> {
        let request = upload_range_request(object, range, body, lease)
            .map_err(|error| attempt_error(OperationClass::PageWrite, error))?;
        transport.execute(cx, &request)
    }

    /// Clears one aligned page range.
    pub fn clear_range<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        object: &ObjectRef,
        range: PageRange,
        lease: Option<&LeaseContext>,
    ) -> Result<ResponseParts, AttemptError> {
        let request = clear_range_request(object, range, lease)
            .map_err(|error| attempt_error(OperationClass::PageWrite, error))?;
        transport.execute(cx, &request)
    }

    /// Updates the service sequence number.
    pub fn update_sequence_number<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        object: &ObjectRef,
        action: SequenceNumberAction,
        lease: Option<&LeaseContext>,
    ) -> Result<ResponseParts, AttemptError> {
        let request = sequence_number_request(object, action, lease);
        transport.execute(cx, &request)
    }

    /// Reads object properties and parses page-object metadata.
    pub fn get_properties<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        object: &ObjectRef,
    ) -> Result<ObjectProperties, AttemptError> {
        let request = properties_request(object);
        let response = transport.execute(cx, &request)?;
        parse_object_properties(&response.metadata)
            .map_err(|error| attempt_error(OperationClass::Metadata, error))
    }

    /// Lists written page ranges, optionally constrained to a byte range.
    pub fn list_written_ranges<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        object: &ObjectRef,
        range: Option<PageRange>,
    ) -> Result<Vec<PageRange>, AttemptError> {
        let request = written_ranges_request(object, range);
        let mut parser = WrittenRangeParser::new();
        transport.execute_with_body_chunks(cx, &request, &mut |chunk| parser.push(&chunk))?;
        parser
            .finish()
            .map_err(|error| attempt_error(OperationClass::PageRead, error))
    }

    /// Reads one aligned page range, delivering body chunks to `on_chunk`.
    ///
    /// If the transport fails after bytes have been delivered, the returned error
    /// is converted to [`ErrorKind::Incomplete`].
    pub fn read_range<T, F>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        object: &ObjectRef,
        range: PageRange,
        mut on_chunk: F,
    ) -> Result<ResponseParts, AttemptError>
    where
        T: Transport,
        F: FnMut(Bytes) -> Result<(), Error>,
    {
        let request = read_range_request(object, range);
        let mut delivered = 0_u64;
        let mut sink_failed = false;
        match transport.execute_with_body_chunks(cx, &request, &mut |chunk| {
            let len = chunk.len() as u64;
            match on_chunk(chunk) {
                Ok(()) => {
                    delivered += len;
                    Ok(())
                }
                Err(error) => {
                    sink_failed = true;
                    Err(error)
                }
            }
        }) {
            Ok(response) => Ok(response),
            Err(error) if sink_failed => Err(error),
            Err(error) if delivered != 0 => Err(AttemptError {
                error: Error::new(
                    ErrorKind::Incomplete,
                    format!("range read failed after delivering {delivered} bytes"),
                ),
                diagnostics: error.diagnostics,
            }),
            Err(error) => Err(error),
        }
    }
}

/// Builds a request to create a page object.
pub fn create_page_request(
    object: &ObjectRef,
    content_len: u64,
    metadata: &MetadataMap,
    lease: Option<&LeaseContext>,
) -> Result<RequestParts, Error> {
    if !content_len.is_multiple_of(PAGE_ALIGNMENT) {
        return Err(Error::new(
            ErrorKind::Range,
            "page object length must be 512-byte aligned",
        ));
    }
    let mut request = RequestParts::new(OperationClass::PageWrite, "PUT", object_uri(object));
    request.metadata.insert("content-length", "0");
    request.metadata.insert("if-none-match", "*");
    request.metadata.insert("x-ms-blob-type", "PageBlob");
    request
        .metadata
        .insert("x-ms-blob-content-length", content_len.to_string());
    apply_user_metadata(&mut request, metadata);
    if let Some(lease) = lease {
        apply_lease(&mut request, lease);
    }
    Ok(request)
}

/// Builds a request to upload an aligned page range.
pub fn upload_range_request(
    object: &ObjectRef,
    range: PageRange,
    body: ReplayBody,
    lease: Option<&LeaseContext>,
) -> Result<RequestParts, Error> {
    upload_range_request_with_conditions(object, range, body, lease, PageWriteConditions::default())
}

/// Builds a request to upload an aligned page range with preconditions.
pub fn upload_range_request_with_conditions(
    object: &ObjectRef,
    range: PageRange,
    body: ReplayBody,
    lease: Option<&LeaseContext>,
    conditions: PageWriteConditions,
) -> Result<RequestParts, Error> {
    let range_len = range.checked_len()?;
    PageRange::aligned_len(range.start(), range_len)?;
    let body_len = usize::try_from(range_len)
        .map_err(|_| Error::new(ErrorKind::Range, "page range length exceeds usize"))?;
    if body.len() != Some(body_len) {
        return Err(Error::new(
            ErrorKind::Range,
            "page upload body length does not match range length",
        ));
    }
    if body.replay() != crate::BodyReplay::Replayable {
        return Err(Error::new(
            ErrorKind::Transport,
            "page upload requires a replayable body",
        ));
    }

    let mut request = RequestParts::new(
        OperationClass::PageWrite,
        "PUT",
        format!("{}?comp=page", object_uri(object)),
    );
    request.metadata.insert("x-ms-page-write", "update");
    request.metadata.insert("x-ms-range", range.header_value());
    request
        .metadata
        .insert("content-length", range_len.to_string());
    if let Some(sequence_number) = conditions.sequence_number_eq {
        request
            .metadata
            .insert("x-ms-if-sequence-number-eq", sequence_number.to_string());
    }
    request.body = body;
    if let Some(lease) = lease {
        apply_lease(&mut request, lease);
    }
    Ok(request)
}

/// Builds a request to clear an aligned page range.
pub fn clear_range_request(
    object: &ObjectRef,
    range: PageRange,
    lease: Option<&LeaseContext>,
) -> Result<RequestParts, Error> {
    PageRange::aligned_len(range.start(), range.checked_len()?)?;
    let mut request = RequestParts::new(
        OperationClass::PageWrite,
        "PUT",
        format!("{}?comp=page", object_uri(object)),
    );
    request.metadata.insert("x-ms-page-write", "clear");
    request.metadata.insert("x-ms-range", range.header_value());
    request.metadata.insert("content-length", "0");
    if let Some(lease) = lease {
        apply_lease(&mut request, lease);
    }
    Ok(request)
}

/// Builds a request to fetch object properties.
pub fn properties_request(object: &ObjectRef) -> RequestParts {
    RequestParts::new(OperationClass::Metadata, "HEAD", object_uri(object))
}

/// Builds a request to list written page ranges.
pub fn written_ranges_request(object: &ObjectRef, range: Option<PageRange>) -> RequestParts {
    let mut request = RequestParts::new(
        OperationClass::PageRead,
        "GET",
        format!("{}?comp=pagelist", object_uri(object)),
    );
    if let Some(range) = range {
        request.metadata.insert("x-ms-range", range.header_value());
    }
    request
}

/// Builds a request to update the page sequence number.
pub fn sequence_number_request(
    object: &ObjectRef,
    action: SequenceNumberAction,
    lease: Option<&LeaseContext>,
) -> RequestParts {
    let mut request = RequestParts::new(
        OperationClass::PageWrite,
        "PUT",
        format!("{}?comp=properties", object_uri(object)),
    );
    request.metadata.insert("content-length", "0");
    request
        .metadata
        .insert("x-ms-sequence-number-action", action.action_header());
    if let SequenceNumberAction::Update(value) = action {
        request
            .metadata
            .insert("x-ms-blob-sequence-number", value.to_string());
    }
    if let Some(lease) = lease {
        apply_lease(&mut request, lease);
    }
    request
}

/// Builds a request to read one page range.
pub fn read_range_request(object: &ObjectRef, range: PageRange) -> RequestParts {
    let mut request = RequestParts::new(OperationClass::PageRead, "GET", object_uri(object));
    request.metadata.insert("x-ms-range", range.header_value());
    request
}

/// Parses object properties from response metadata.
pub fn parse_object_properties(metadata: &MetadataMap) -> Result<ObjectProperties, Error> {
    let content_len = parse_u64_header(metadata, "content-length")?.unwrap_or(0);
    let sequence_number = parse_u64_header(metadata, "x-ms-blob-sequence-number")?;
    let mut object_metadata = MetadataMap::new();
    for (name, value) in metadata.entries() {
        if let Some(stripped) = name.strip_prefix("x-ms-meta-") {
            object_metadata.insert(stripped, value);
        }
    }
    Ok(ObjectProperties {
        content_len,
        etag: metadata.get("etag").map(str::to_owned),
        sequence_number,
        snapshot: metadata.get("x-ms-snapshot").map(str::to_owned),
        metadata: object_metadata,
    })
}

/// Parses and coalesces written page ranges from service XML.
pub fn parse_written_ranges(xml: &str) -> Result<Vec<PageRange>, Error> {
    let mut ranges = Vec::new();
    let mut rest = xml;
    while let Some(range_start) = rest.find("<PageRange>") {
        rest = &rest[range_start + "<PageRange>".len()..];
        let Some(range_end) = rest.find("</PageRange>") else {
            return Err(Error::new(ErrorKind::Corruption, "unterminated page range"));
        };
        let range = &rest[..range_end];
        let start = parse_xml_u64(range, "Start")?;
        let end = parse_xml_u64(range, "End")?;
        ranges.push(PageRange::new(start, end)?);
        rest = &rest[range_end + "</PageRange>".len()..];
    }
    Ok(coalesce_ranges(ranges))
}

/// Sorts ranges and merges overlapping or adjacent entries.
pub fn coalesce_ranges(mut ranges: Vec<PageRange>) -> Vec<PageRange> {
    ranges.sort();
    let mut coalesced: Vec<PageRange> = Vec::new();
    for range in ranges {
        let Some(last) = coalesced.last_mut() else {
            coalesced.push(range);
            continue;
        };
        if range.start <= last.end.saturating_add(1) {
            last.end = last.end.max(range.end);
        } else {
            coalesced.push(range);
        }
    }
    coalesced
}

struct WrittenRangeParser {
    buffer: String,
    ranges: Vec<PageRange>,
}

impl WrittenRangeParser {
    fn new() -> Self {
        Self {
            buffer: String::new(),
            ranges: Vec::new(),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<(), Error> {
        let text = std::str::from_utf8(chunk).map_err(|_| {
            Error::new(ErrorKind::Corruption, "written range response is not UTF-8")
        })?;
        self.buffer.push_str(text);
        while let Some(start) = self.buffer.find("<PageRange>") {
            let Some(end) = self.buffer[start..].find("</PageRange>") else {
                if start > 0 {
                    self.buffer.drain(..start);
                }
                return Ok(());
            };
            let end = start + end + "</PageRange>".len();
            let range_xml = self.buffer[start..end].to_owned();
            let range = parse_written_ranges(&range_xml)?;
            for range in range {
                push_coalesced(&mut self.ranges, range);
            }
            self.buffer.drain(..end);
        }
        if self.buffer.len() > 16 * 1024 {
            let keep_from = self.buffer.len() - 16 * 1024;
            self.buffer.drain(..keep_from);
        }
        Ok(())
    }

    fn finish(self) -> Result<Vec<PageRange>, Error> {
        if self.buffer.contains("<PageRange>") {
            Err(Error::new(ErrorKind::Corruption, "unterminated page range"))
        } else {
            Ok(self.ranges)
        }
    }
}

fn push_coalesced(ranges: &mut Vec<PageRange>, range: PageRange) {
    let Some(last) = ranges.last_mut() else {
        ranges.push(range);
        return;
    };
    if range.start <= last.end.saturating_add(1) {
        last.end = last.end.max(range.end);
    } else {
        ranges.push(range);
    }
}

fn parse_u64_header(metadata: &MetadataMap, key: &str) -> Result<Option<u64>, Error> {
    metadata
        .get(key)
        .map(|value| {
            value.parse::<u64>().map_err(|_| {
                Error::new(
                    ErrorKind::MetadataFormat,
                    format!("header {key} is not an unsigned integer"),
                )
            })
        })
        .transpose()
}

fn parse_xml_u64(xml: &str, tag: &str) -> Result<u64, Error> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let Some(start) = xml.find(&open) else {
        return Err(Error::new(
            ErrorKind::Corruption,
            "page range missing bound",
        ));
    };
    let after_start = start + open.len();
    let Some(end) = xml[after_start..].find(&close) else {
        return Err(Error::new(
            ErrorKind::Corruption,
            "page range has unterminated bound",
        ));
    };
    xml[after_start..after_start + end]
        .trim()
        .parse::<u64>()
        .map_err(|_| Error::new(ErrorKind::Corruption, "page range bound is invalid"))
}

fn apply_user_metadata(request: &mut RequestParts, metadata: &MetadataMap) {
    for (name, value) in metadata.entries() {
        request.metadata.insert(format!("x-ms-meta-{name}"), value);
    }
}

fn object_uri(object: &ObjectRef) -> String {
    format!("/{}", object.encoded_path())
}

fn attempt_error(operation: OperationClass, error: Error) -> AttemptError {
    AttemptError {
        error,
        diagnostics: Diagnostics::new(operation),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AccountId, AttemptDiagnostics, ContainerName, ObjectKind, ObjectName};

    #[derive(Clone)]
    struct RecordingTransport {
        requests: Vec<RequestParts>,
        response: Result<ResponseParts, AttemptError>,
        chunks: Vec<Bytes>,
        fail_after_chunks: Option<AttemptError>,
    }

    impl RecordingTransport {
        fn ok() -> Self {
            Self {
                requests: Vec::new(),
                response: Ok(ResponseParts {
                    status: 200,
                    metadata: MetadataMap::new(),
                    diagnostics: Diagnostics::new(OperationClass::PageRead),
                }),
                chunks: Vec::new(),
                fail_after_chunks: None,
            }
        }
    }

    impl Transport for RecordingTransport {
        fn execute(
            &mut self,
            _cx: &kimojio_stack::RuntimeContext<'_>,
            request: &RequestParts,
        ) -> Result<ResponseParts, AttemptError> {
            self.requests.push(request.clone());
            self.response.clone()
        }

        fn execute_with_body_chunks(
            &mut self,
            _cx: &kimojio_stack::RuntimeContext<'_>,
            request: &RequestParts,
            on_chunk: &mut dyn FnMut(Bytes) -> Result<(), Error>,
        ) -> Result<ResponseParts, AttemptError> {
            self.requests.push(request.clone());
            for chunk in &self.chunks {
                on_chunk(chunk.clone()).map_err(|error| AttemptError {
                    error,
                    diagnostics: Diagnostics::new(request.operation),
                })?;
            }
            if let Some(error) = &self.fail_after_chunks {
                return Err(error.clone());
            }
            self.response.clone()
        }
    }

    fn object() -> ObjectRef {
        ObjectRef {
            account: AccountId::new("acct"),
            container: ContainerName::new("container"),
            name: ObjectName::new("object"),
            kind: ObjectKind::Data,
        }
    }

    #[test]
    fn page_requests_validate_alignment_and_set_headers() {
        let mut metadata = MetadataMap::new();
        metadata.insert("high-water-lsn", "42");
        let lease = LeaseContext {
            lease_id: "lease".into(),
            epoch: 2,
        };
        let create = create_page_request(&object(), 1024, &metadata, Some(&lease)).unwrap();

        assert_eq!(create.uri, "/acct/container/object");
        assert_eq!(create.metadata.get("content-length"), Some("0"));
        assert_eq!(create.metadata.get("if-none-match"), Some("*"));
        assert_eq!(create.metadata.get("x-ms-blob-type"), Some("PageBlob"));
        assert_eq!(create.metadata.get("x-ms-meta-high-water-lsn"), Some("42"));
        assert_eq!(create.metadata.get("x-ms-lease-id"), Some("lease"));

        let range = PageRange::aligned_len(0, 512).unwrap();
        let upload =
            upload_range_request(&object(), range, ReplayBody::from_vec(vec![7; 512]), None)
                .unwrap();
        assert_eq!(upload.metadata.get("x-ms-page-write"), Some("update"));
        assert_eq!(upload.metadata.get("x-ms-range"), Some("bytes=0-511"));
        assert_eq!(upload.metadata.get("content-length"), Some("512"));
        let conditional_upload = upload_range_request_with_conditions(
            &object(),
            range,
            ReplayBody::from_vec(vec![7; 512]),
            None,
            PageWriteConditions::default().with_sequence_number_eq(9),
        )
        .unwrap();
        assert_eq!(
            conditional_upload
                .metadata
                .get("x-ms-if-sequence-number-eq"),
            Some("9")
        );
        assert_eq!(
            upload_range_request(
                &object(),
                PageRange::new(1, 512).unwrap(),
                ReplayBody::Empty,
                None
            )
            .unwrap_err()
            .kind(),
            ErrorKind::Range
        );
    }

    #[test]
    fn page_client_executes_clear_sequence_properties_and_streaming_read() {
        let mut response_metadata = MetadataMap::new();
        response_metadata.insert("content-length", "4096");
        response_metadata.insert("etag", "etag-1");
        response_metadata.insert("x-ms-blob-sequence-number", "9");
        response_metadata.insert("x-ms-meta-ownership-epoch", "3");
        let mut transport = RecordingTransport {
            response: Ok(ResponseParts {
                status: 200,
                metadata: response_metadata,
                diagnostics: Diagnostics::new(OperationClass::Metadata),
            }),
            ..RecordingTransport::ok()
        };

        let properties = kimojio_stack::Runtime::new()
            .block_on(|cx| PageClient.get_properties(cx, &mut transport, &object()))
            .unwrap();

        assert_eq!(properties.content_len, 4096);
        assert_eq!(properties.sequence_number, Some(9));
        assert_eq!(properties.metadata.get("ownership-epoch"), Some("3"));

        let clear = clear_range_request(&object(), PageRange::aligned_len(512, 512).unwrap(), None)
            .unwrap();
        assert_eq!(clear.metadata.get("x-ms-page-write"), Some("clear"));
        assert_eq!(clear.metadata.get("content-length"), Some("0"));
        let sequence = sequence_number_request(&object(), SequenceNumberAction::Update(11), None);
        assert_eq!(
            sequence.metadata.get("x-ms-sequence-number-action"),
            Some("update")
        );
        assert_eq!(
            sequence.metadata.get("x-ms-blob-sequence-number"),
            Some("11")
        );

        let increment = sequence_number_request(&object(), SequenceNumberAction::Increment, None);
        assert_eq!(increment.metadata.get("content-length"), Some("0"));
        assert_eq!(
            increment.metadata.get("x-ms-sequence-number-action"),
            Some("increment")
        );
        assert_eq!(
            classify_sequence_number_failure(&Error::new(
                ErrorKind::SequenceNumber,
                "sequence number overflow"
            )),
            SequenceNumberRecovery::RefreshProperties
        );
        assert_eq!(
            classify_page_write_failure(&Error::new(ErrorKind::Unavailable, "ambiguous write")),
            PageWriteRecovery::RefreshPropertiesBeforeRetry
        );
    }

    #[test]
    fn written_ranges_parse_and_coalesce() {
        let xml = r#"
            <PageList>
              <PageRange><Start>512</Start><End>1023</End></PageRange>
              <PageRange><Start>0</Start><End>511</End></PageRange>
              <PageRange><Start>2048</Start><End>2559</End></PageRange>
            </PageList>
        "#;

        let ranges = parse_written_ranges(xml).unwrap();

        assert_eq!(
            ranges,
            vec![
                PageRange::new(0, 1023).unwrap(),
                PageRange::new(2048, 2559).unwrap()
            ]
        );
    }

    #[test]
    fn list_and_read_ranges_use_streaming_transport() {
        let mut list_transport = RecordingTransport {
            chunks: vec![Bytes::from_static(
                b"<PageList><PageRange><Start>0</Start><End>511</End></PageRange></PageList>",
            )],
            ..RecordingTransport::ok()
        };
        let ranges = kimojio_stack::Runtime::new()
            .block_on(|cx| PageClient.list_written_ranges(cx, &mut list_transport, &object(), None))
            .unwrap();
        assert_eq!(ranges, vec![PageRange::new(0, 511).unwrap()]);

        let mut diagnostics = Diagnostics::new(OperationClass::PageRead);
        diagnostics.push_attempt(AttemptDiagnostics {
            attempt: 1,
            status: None,
            service_code: None,
            request_id: Some("req".into()),
            elapsed: None,
            retriable: true,
        });
        let mut read_transport = RecordingTransport {
            chunks: vec![Bytes::from_static(b"partial")],
            fail_after_chunks: Some(AttemptError {
                error: Error::new(ErrorKind::Unavailable, "busy"),
                diagnostics,
            }),
            ..RecordingTransport::ok()
        };
        let mut received = Vec::new();
        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| {
                PageClient.read_range(
                    cx,
                    &mut read_transport,
                    &object(),
                    PageRange::new(0, 6).unwrap(),
                    |chunk| {
                        received.extend_from_slice(&chunk);
                        Ok(())
                    },
                )
            })
            .unwrap_err();

        assert_eq!(received, b"partial");
        assert_eq!(error.error.kind(), ErrorKind::Incomplete);
        assert_eq!(
            read_transport.requests[0].metadata.get("x-ms-range"),
            Some("bytes=0-6")
        );
    }

    #[test]
    fn read_range_preserves_caller_sink_failures() {
        let mut read_transport = RecordingTransport {
            chunks: vec![Bytes::from_static(b"chunk")],
            ..RecordingTransport::ok()
        };
        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| {
                PageClient.read_range(
                    cx,
                    &mut read_transport,
                    &object(),
                    PageRange::new(0, 4).unwrap(),
                    |_| Err(Error::new(ErrorKind::Corruption, "sink failed")),
                )
            })
            .unwrap_err();

        assert_eq!(error.error.kind(), ErrorKind::Corruption);
    }

    #[test]
    fn page_range_reports_unrepresentable_lengths() {
        let range = PageRange::new(0, u64::MAX).unwrap();
        assert_eq!(range.checked_len().unwrap_err().kind(), ErrorKind::Range);
    }
}
