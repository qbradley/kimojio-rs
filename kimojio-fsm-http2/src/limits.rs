/// Default maximum aggregate encoded header size.
const DEFAULT_MAX_HEADER_BYTES: usize = 64 * 1024;
/// Default maximum number of header occurrences.
const DEFAULT_MAX_HEADERS: usize = 100;
/// Default maximum buffered message body size.
const DEFAULT_MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Default maximum number of concurrently active HTTP/2 streams.
pub(crate) const DEFAULT_MAX_ACTIVE_STREAMS: usize = 100;
/// Default maximum number of HTTP/1 requests served by one connection.
///
/// One thousand amortizes setup for ordinary pooled clients while placing a
/// finite bound on connection lifetime and per-peer request monopolization.
pub const DEFAULT_MAX_REQUESTS_PER_CONNECTION: usize = 1_000;

/// HTTP message and connection resource limits enforced by the protocol state machines.
///
/// Connection drivers apply the body limit to accumulated bodies. Their
/// explicit streaming modes retain framing and flow-control bounds without
/// imposing this buffered-memory limit on total streamed bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HttpLimits {
    max_header_bytes: usize,
    max_headers: usize,
    max_body_bytes: usize,
    max_active_streams: usize,
    max_requests_per_connection: usize,
}

impl HttpLimits {
    /// Creates the default HTTP resource limits.
    pub const fn new() -> Self {
        Self {
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            max_headers: DEFAULT_MAX_HEADERS,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_active_streams: DEFAULT_MAX_ACTIVE_STREAMS,
            max_requests_per_connection: DEFAULT_MAX_REQUESTS_PER_CONNECTION,
        }
    }

    /// Returns the maximum aggregate encoded header size.
    pub const fn max_header_bytes(&self) -> usize {
        self.max_header_bytes
    }

    /// Sets the maximum aggregate encoded header size.
    pub const fn set_max_header_bytes(mut self, value: usize) -> Self {
        self.max_header_bytes = value;
        self
    }

    /// Returns the maximum number of header occurrences.
    pub const fn max_headers(&self) -> usize {
        self.max_headers
    }

    /// Sets the maximum number of header occurrences.
    pub const fn set_max_headers(mut self, value: usize) -> Self {
        self.max_headers = value;
        self
    }

    /// Returns the maximum buffered message body size.
    pub const fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }

    /// Sets the maximum buffered message body size.
    pub const fn set_max_body_bytes(mut self, value: usize) -> Self {
        self.max_body_bytes = value;
        self
    }

    /// Returns the maximum number of concurrently active HTTP/2 streams.
    pub const fn max_active_streams(&self) -> usize {
        self.max_active_streams
    }

    /// Sets the maximum number of concurrently active HTTP/2 streams.
    pub const fn set_max_active_streams(mut self, value: usize) -> Self {
        self.max_active_streams = value;
        self
    }

    /// Returns the maximum number of HTTP/1 requests served by one connection.
    pub const fn max_requests_per_connection(&self) -> usize {
        self.max_requests_per_connection
    }

    /// Sets the maximum number of HTTP/1 requests served by one connection.
    pub const fn set_max_requests_per_connection(mut self, value: usize) -> Self {
        self.max_requests_per_connection = value;
        self
    }
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self::new()
    }
}

/// HPACK's per-entry accounting overhead from RFC 7541 section 4.1.
pub const fn hpack_entry_overhead() -> usize {
    32
}

/// Returns the RFC 7541 header-list size of one HPACK field.
pub fn hpack_field_size(name: &[u8], value: &[u8]) -> usize {
    name.len()
        .saturating_add(value.len())
        .saturating_add(hpack_entry_overhead())
}

/// Parses a `content-length` field value, including equivalent comma-list values.
///
/// Empty, malformed, conflicting, or overflowing values are rejected.
pub fn parse_content_length(value: &[u8]) -> Option<usize> {
    let mut content_length = None;
    for item in value.split(|byte| *byte == b',') {
        let item = item.trim_ascii();
        if item.is_empty() {
            return None;
        }
        let mut parsed = 0usize;
        for &byte in item {
            if !byte.is_ascii_digit() {
                return None;
            }
            parsed = parsed
                .checked_mul(10)?
                .checked_add(usize::from(byte - b'0'))?;
        }
        if content_length
            .replace(parsed)
            .is_some_and(|existing| existing != parsed)
        {
            return None;
        }
    }
    content_length
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hpack_field_size_includes_entry_overhead() {
        assert_eq!(hpack_entry_overhead(), 32);
        assert_eq!(hpack_field_size(b"x", b"y"), 34);
    }

    #[test]
    fn parses_content_length_values() {
        assert_eq!(parse_content_length(b"42"), Some(42));
        assert_eq!(parse_content_length(b" 42, 42 "), Some(42));
        assert_eq!(parse_content_length(b"42,43"), None);
        assert_eq!(parse_content_length(b""), None);
        assert_eq!(parse_content_length(b"42,"), None);
        assert_eq!(parse_content_length(b"+42"), None);
        assert_eq!(parse_content_length(b"4x"), None);

        let overflow = format!("{}0", usize::MAX);
        assert_eq!(parse_content_length(overflow.as_bytes()), None);
    }

    #[test]
    fn configures_the_http1_request_limit() {
        let defaults = HttpLimits::new();
        assert_eq!(
            defaults.max_requests_per_connection(),
            DEFAULT_MAX_REQUESTS_PER_CONNECTION
        );
        assert_eq!(
            defaults
                .set_max_requests_per_connection(17)
                .max_requests_per_connection(),
            17
        );
    }
}
