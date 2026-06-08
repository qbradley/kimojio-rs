// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Container listing helpers.
//!
//! Listing is page-oriented. [`ListClient::fetch_page`] fetches one page,
//! [`ListClient::stream_pages`] follows continuation tokens, and
//! [`merge_dedup_pages`] can combine pages while preserving one item per
//! `(name, snapshot)` key.
//!
//! ```
//! use kimojio_stack_storage::{AccountId, ContainerName, ListOptions, list_request};
//!
//! let options = ListOptions {
//!     prefix: Some("tenant/".into()),
//!     include_metadata: true,
//!     max_results: Some(100),
//!     ..ListOptions::default()
//! };
//! let request = list_request(&AccountId::new("acct"), &ContainerName::new("container"), &options);
//! assert!(request.uri.contains("comp=list"));
//! ```

use std::collections::BTreeMap;
use std::time::Instant;

use bytes::BytesMut;

use crate::{
    AccountId, AttemptError, ContainerName, Diagnostics, Error, ErrorKind, MetadataMap, ObjectKind,
    ObjectName, OperationClass, ReplayBody, RequestParts, RetryDecision, RetryPolicy, RetryState,
    Transport, model::percent_encode_component,
};

/// Options controlling one list page request.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ListOptions {
    /// Optional object-name prefix.
    pub prefix: Option<String>,
    /// Continuation marker from a previous [`ListPage`].
    pub continuation: Option<String>,
    /// Whether to ask the service to include metadata.
    pub include_metadata: bool,
    /// Whether to include snapshots in the listing.
    pub include_snapshots: bool,
    /// Optional object-kind filter. Requires metadata to be present.
    pub kind: Option<ObjectKind>,
    /// Optional service-side maximum number of results.
    pub max_results: Option<u32>,
}

/// One item returned by a list operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListItem {
    /// Object name.
    pub name: ObjectName,
    /// Parsed object kind.
    pub kind: ObjectKind,
    /// Parsed object metadata.
    pub metadata: MetadataMap,
    /// Optional snapshot identifier.
    pub snapshot: Option<String>,
}

/// One page returned by a list operation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ListPage {
    /// Items in this page after filtering.
    pub items: Vec<ListItem>,
    /// Continuation marker for the next page.
    pub continuation: Option<String>,
}

/// Client helper for container listing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ListClient;

impl ListClient {
    /// Fetches and parses one list page.
    pub fn fetch_page<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        account: &AccountId,
        container: &ContainerName,
        options: &ListOptions,
    ) -> Result<ListPage, AttemptError> {
        let request = list_request(account, container, options);
        let mut body = BytesMut::new();
        transport.execute_with_body_chunks(cx, &request, &mut |chunk| {
            body.extend_from_slice(&chunk);
            Ok(())
        })?;
        let text = std::str::from_utf8(&body).map_err(|_| {
            attempt_error(Error::new(
                ErrorKind::Corruption,
                "list response is not UTF-8",
            ))
        })?;
        parse_list_page(text, options.kind).map_err(attempt_error)
    }

    /// Fetches one page with retry policy applied to retryable list failures.
    pub fn fetch_page_with_retry<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        account: &AccountId,
        container: &ContainerName,
        options: &ListOptions,
        retry: RetryPolicy,
    ) -> Result<ListPage, AttemptError> {
        let mut attempt = 1;
        let start = Instant::now();
        loop {
            match self.fetch_page(cx, transport, account, container, options) {
                Ok(page) => return Ok(page),
                Err(error) if is_retryable_list_error(&error.error) => {
                    let elapsed = start.elapsed();
                    let decision = retry.decide_with_state(
                        attempt,
                        OperationClass::List,
                        &ReplayBody::Empty,
                        &error.error,
                        None,
                        RetryState {
                            elapsed,
                            canceled: false,
                            jitter: std::time::Duration::ZERO,
                        },
                    );
                    match decision {
                        RetryDecision::RetryAfter(delay) => {
                            if !delay.is_zero() {
                                cx.sleep(delay).map_err(|errno| {
                                    attempt_error(Error::new(
                                        ErrorKind::Transport,
                                        format!("list retry sleep failed: {errno:?}"),
                                    ))
                                })?;
                            }
                            attempt += 1;
                        }
                        RetryDecision::DoNotRetry => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Follows continuations and calls `on_page` for each page.
    ///
    /// The callback runs before the next page is fetched, allowing callers to
    /// apply backpressure or stop by returning an error.
    pub fn stream_pages<T, F>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        account: &AccountId,
        container: &ContainerName,
        options: &ListOptions,
        mut on_page: F,
    ) -> Result<(), AttemptError>
    where
        T: Transport,
        F: FnMut(ListPage) -> Result<(), Error>,
    {
        let mut next = Some(options.clone());
        while let Some(current) = next.take() {
            let page = self.fetch_page(cx, transport, account, container, &current)?;
            let continuation = page.continuation.clone();
            on_page(page).map_err(attempt_error)?;
            next = continuation.map(|continuation| ListOptions {
                continuation: Some(continuation),
                ..current
            });
        }
        Ok(())
    }
}

/// Builds a list request for one container page.
pub fn list_request(
    account: &AccountId,
    container: &ContainerName,
    options: &ListOptions,
) -> RequestParts {
    let mut query = vec!["restype=container".to_string(), "comp=list".to_string()];
    if let Some(prefix) = &options.prefix {
        query.push(format!("prefix={}", percent_encode(prefix)));
    }
    if let Some(continuation) = &options.continuation {
        query.push(format!("marker={}", percent_encode(continuation)));
    }
    let include_metadata = options.include_metadata || options.kind.is_some();
    if include_metadata || options.include_snapshots {
        let mut include = Vec::new();
        if include_metadata {
            include.push("metadata");
        }
        if options.include_snapshots {
            include.push("snapshots");
        }
        query.push(format!("include={}", include.join(",")));
    }
    if let Some(max_results) = options.max_results {
        query.push(format!("maxresults={max_results}"));
    }
    RequestParts::new(
        OperationClass::List,
        "GET",
        format!(
            "/{}/{}?{}",
            percent_encode_component(account.as_str(), false),
            percent_encode_component(container.as_str(), false),
            query.join("&")
        ),
    )
}

/// Parses service XML into a list page.
///
/// If `kind_filter` is supplied, each item must include `object-kind` metadata
/// so filtering can be validated rather than silently defaulted.
pub fn parse_list_page(xml: &str, kind_filter: Option<ObjectKind>) -> Result<ListPage, Error> {
    let continuation = tag_text(xml, "NextMarker")?.filter(|value| !value.is_empty());
    let mut items = Vec::new();
    let mut rest = xml;
    while let Some((item_xml, next_rest)) = take_next_item(rest)? {
        rest = next_rest;
        let Some(name) = tag_text(item_xml, "Name")? else {
            return Err(Error::new(
                ErrorKind::Corruption,
                "listed item missing name",
            ));
        };
        let metadata = parse_metadata_block(item_xml)?;
        let kind = match metadata.get("object-kind").and_then(parse_object_kind) {
            Some(kind) => kind,
            None if kind_filter.is_some() => {
                return Err(Error::new(
                    ErrorKind::MetadataFormat,
                    "listed item is missing object-kind metadata required for filtering",
                ));
            }
            None => ObjectKind::Data,
        };
        if kind_filter.is_some_and(|expected| expected != kind) {
            continue;
        }
        items.push(ListItem {
            name: ObjectName::new(name),
            kind,
            metadata,
            snapshot: tag_text(item_xml, "Snapshot")?,
        });
    }
    Ok(ListPage {
        items,
        continuation,
    })
}

/// Merges pages and de-duplicates by object name plus snapshot identifier.
pub fn merge_dedup_pages(pages: &[ListPage]) -> Vec<ListItem> {
    let mut merged = BTreeMap::<(String, Option<String>), ListItem>::new();
    for page in pages {
        for item in &page.items {
            merged
                .entry((item.name.as_str().to_owned(), item.snapshot.clone()))
                .or_insert_with(|| item.clone());
        }
    }
    merged.into_values().collect()
}

/// Returns whether a list error is eligible for retry.
pub fn is_retryable_list_error(error: &Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::Timeout | ErrorKind::Transport | ErrorKind::Unavailable
    )
}

fn take_next_item(xml: &str) -> Result<Option<(&str, &str)>, Error> {
    let Some(start) = xml.find("<Blob>").or_else(|| xml.find("<Object>")) else {
        return Ok(None);
    };
    let (open, close) = if xml[start..].starts_with("<Blob>") {
        ("<Blob>", "</Blob>")
    } else {
        ("<Object>", "</Object>")
    };
    let body_start = start + open.len();
    let Some(end) = xml[body_start..].find(close) else {
        return Err(Error::new(
            ErrorKind::Corruption,
            "unterminated listed item",
        ));
    };
    let body_end = body_start + end;
    Ok(Some((
        &xml[body_start..body_end],
        &xml[body_end + close.len()..],
    )))
}

fn parse_metadata_block(xml: &str) -> Result<MetadataMap, Error> {
    let mut metadata = MetadataMap::new();
    let Some(start) = xml.find("<Metadata>") else {
        return Ok(metadata);
    };
    let body_start = start + "<Metadata>".len();
    let Some(end) = xml[body_start..].find("</Metadata>") else {
        return Err(Error::new(ErrorKind::Corruption, "unterminated metadata"));
    };
    let mut rest = &xml[body_start..body_start + end];
    while let Some(start) = rest.find('<') {
        if rest[start..].starts_with("</") {
            break;
        }
        let Some(tag_end) = rest[start + 1..].find('>') else {
            return Err(Error::new(
                ErrorKind::Corruption,
                "unterminated metadata tag",
            ));
        };
        let key = &rest[start + 1..start + 1 + tag_end];
        let close = format!("</{key}>");
        let value_start = start + tag_end + 2;
        let Some(value_end) = rest[value_start..].find(&close) else {
            return Err(Error::new(
                ErrorKind::Corruption,
                "unterminated metadata value",
            ));
        };
        metadata.insert(
            key,
            xml_unescape(&rest[value_start..value_start + value_end])?,
        );
        rest = &rest[value_start + value_end + close.len()..];
    }
    Ok(metadata)
}

fn tag_text(xml: &str, tag: &str) -> Result<Option<String>, Error> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let Some(start) = xml.find(&open) else {
        return Ok(None);
    };
    let body_start = start + open.len();
    let Some(end) = xml[body_start..].find(&close) else {
        return Err(Error::new(ErrorKind::Corruption, "unterminated XML tag"));
    };
    xml_unescape(&xml[body_start..body_start + end]).map(Some)
}

fn xml_unescape(value: &str) -> Result<String, Error> {
    let mut decoded = String::new();
    let mut rest = value;
    while let Some(index) = rest.find('&') {
        decoded.push_str(&rest[..index]);
        rest = &rest[index..];
        if let Some(next) = rest.strip_prefix("&amp;") {
            decoded.push('&');
            rest = next;
        } else if let Some(next) = rest.strip_prefix("&lt;") {
            decoded.push('<');
            rest = next;
        } else if let Some(next) = rest.strip_prefix("&gt;") {
            decoded.push('>');
            rest = next;
        } else if let Some(next) = rest.strip_prefix("&quot;") {
            decoded.push('"');
            rest = next;
        } else if let Some(next) = rest.strip_prefix("&apos;") {
            decoded.push('\'');
            rest = next;
        } else {
            return Err(Error::new(ErrorKind::Corruption, "unsupported XML entity"));
        }
    }
    decoded.push_str(rest);
    Ok(decoded)
}

fn parse_object_kind(value: &str) -> Option<ObjectKind> {
    match value {
        "internal" => Some(ObjectKind::Internal),
        "metadata" => Some(ObjectKind::Metadata),
        "data" => Some(ObjectKind::Data),
        "backup" => Some(ObjectKind::Backup),
        "status" => Some(ObjectKind::Status),
        "config" => Some(ObjectKind::Config),
        "test" => Some(ObjectKind::Test),
        _ => None,
    }
}

pub(crate) fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn attempt_error(error: Error) -> AttemptError {
    AttemptError {
        error,
        diagnostics: Diagnostics::new(OperationClass::List),
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use crate::ResponseParts;

    use super::*;

    #[derive(Default)]
    struct PagedTransport {
        requests: Vec<RequestParts>,
        pages: Vec<Result<Vec<Bytes>, AttemptError>>,
    }

    impl Transport for PagedTransport {
        fn execute(
            &mut self,
            cx: &kimojio_stack::RuntimeContext<'_>,
            request: &RequestParts,
        ) -> Result<ResponseParts, AttemptError> {
            let mut discard = |_| Ok(());
            self.execute_with_body_chunks(cx, request, &mut discard)
        }

        fn execute_with_body_chunks(
            &mut self,
            _cx: &kimojio_stack::RuntimeContext<'_>,
            request: &RequestParts,
            on_chunk: &mut dyn FnMut(Bytes) -> Result<(), Error>,
        ) -> Result<ResponseParts, AttemptError> {
            self.requests.push(request.clone());
            let chunks = self.pages.remove(0)?;
            for chunk in chunks {
                on_chunk(chunk).map_err(attempt_error)?;
            }
            Ok(ResponseParts {
                status: 200,
                metadata: MetadataMap::new(),
                diagnostics: Diagnostics::new(OperationClass::List),
            })
        }
    }

    #[test]
    fn list_request_encodes_prefix_marker_include_and_limit() {
        let request = list_request(
            &AccountId::new("acct"),
            &ContainerName::new("container"),
            &ListOptions {
                prefix: Some("a b/".into()),
                continuation: Some("next".into()),
                include_metadata: true,
                include_snapshots: true,
                max_results: Some(10),
                kind: None,
            },
        );

        assert!(request.uri.contains("prefix=a%20b/"));
        assert!(request.uri.contains("marker=next"));
        assert!(request.uri.contains("include=metadata,snapshots"));
        assert!(request.uri.contains("maxresults=10"));

        let filtered = list_request(
            &AccountId::new("acct"),
            &ContainerName::new("container"),
            &ListOptions {
                kind: Some(ObjectKind::Metadata),
                ..ListOptions::default()
            },
        );
        assert!(filtered.uri.contains("include=metadata"));
    }

    #[test]
    fn list_page_parses_filters_and_merges_deduped_items() {
        let xml = r#"
            <EnumerationResults>
              <Blobs>
                <Blob><Name>a</Name><Metadata><object-kind>data</object-kind></Metadata></Blob>
                <Blob><Name>b&amp;c</Name><Snapshot>s1</Snapshot><Metadata><object-kind>metadata</object-kind></Metadata></Blob>
              </Blobs>
              <NextMarker>next</NextMarker>
            </EnumerationResults>
        "#;
        let page = parse_list_page(xml, Some(ObjectKind::Data)).unwrap();

        assert_eq!(page.continuation.as_deref(), Some("next"));
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].name.as_str(), "a");
        let metadata_page = parse_list_page(xml, Some(ObjectKind::Metadata)).unwrap();
        assert_eq!(metadata_page.items[0].name.as_str(), "b&c");

        let merged = merge_dedup_pages(&[
            page.clone(),
            ListPage {
                items: vec![page.items[0].clone()],
                continuation: None,
            },
        ]);
        assert_eq!(merged.len(), 1);

        let missing_kind = parse_list_page(
            "<EnumerationResults><Blobs><Blob><Name>a</Name></Blob></Blobs></EnumerationResults>",
            Some(ObjectKind::Metadata),
        )
        .unwrap_err();
        assert_eq!(missing_kind.kind(), ErrorKind::MetadataFormat);
    }

    #[test]
    fn stream_pages_preserves_delivered_pages_before_continuation_failure() {
        let first = Bytes::from_static(
            b"<EnumerationResults><Blobs><Blob><Name>a</Name></Blob></Blobs><NextMarker>next</NextMarker></EnumerationResults>",
        );
        let second = Bytes::from_static(b"<EnumerationResults><Blobs><Blob><Name>b</Name>");
        let mut transport = PagedTransport {
            requests: Vec::new(),
            pages: vec![Ok(vec![first]), Ok(vec![second])],
        };
        let mut delivered = Vec::new();
        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| {
                ListClient.stream_pages(
                    cx,
                    &mut transport,
                    &AccountId::new("acct"),
                    &ContainerName::new("container"),
                    &ListOptions::default(),
                    |page| {
                        delivered.push(page.items[0].name.as_str().to_owned());
                        Ok(())
                    },
                )
            })
            .unwrap_err();

        assert_eq!(delivered, vec!["a"]);
        assert_eq!(error.error.kind(), ErrorKind::Corruption);
        assert!(transport.requests[1].uri.contains("marker=next"));
    }

    #[test]
    fn retryable_list_errors_are_explicit() {
        assert!(is_retryable_list_error(&Error::new(
            ErrorKind::Unavailable,
            "busy"
        )));
        assert!(!is_retryable_list_error(&Error::new(
            ErrorKind::Authorization,
            "forbidden"
        )));
    }

    #[test]
    fn fetch_page_with_retry_retries_retryable_failures() {
        let mut transport = PagedTransport {
            requests: Vec::new(),
            pages: vec![
                Err(attempt_error(Error::new(ErrorKind::Unavailable, "busy"))),
                Ok(vec![Bytes::from_static(
                    b"<EnumerationResults><Blobs><Blob><Name>a</Name></Blob></Blobs></EnumerationResults>",
                )]),
            ],
        };
        let page = kimojio_stack::Runtime::new()
            .block_on(|cx| {
                ListClient.fetch_page_with_retry(
                    cx,
                    &mut transport,
                    &AccountId::new("acct"),
                    &ContainerName::new("container"),
                    &ListOptions::default(),
                    RetryPolicy {
                        max_attempts: 2,
                        base_delay: std::time::Duration::ZERO,
                        max_delay: std::time::Duration::ZERO,
                        max_jitter: std::time::Duration::ZERO,
                        total_timeout: std::time::Duration::from_secs(1),
                    },
                )
            })
            .unwrap();

        assert_eq!(page.items[0].name.as_str(), "a");
        assert_eq!(transport.requests.len(), 2);
    }
}
