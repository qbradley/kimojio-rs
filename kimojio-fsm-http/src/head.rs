use crate::server::{
    H2HeaderValidationRole, content_length_from_raw_headers, enforce_h2_field_limits,
    validate_decoded_header_fields,
};
use crate::{H2HeaderField, HttpLimits, ServerError};

/// A supported HTTP/1 wire version.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Http1Version {
    /// HTTP/1.0.
    Http10,
    /// HTTP/1.1.
    Http11,
}

/// Maps an HTTP/1 minor version number to its wire form, rejecting unsupported versions.
pub const fn http1_version(minor: u8) -> Result<Http1Version, ServerError> {
    match minor {
        0 => Ok(Http1Version::Http10),
        1 => Ok(Http1Version::Http11),
        _ => Err(ServerError::UnsupportedVersion),
    }
}

/// A validated, borrowed HTTP/2 request head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2RequestHeadRef<'a> {
    method: &'a [u8],
    scheme: Option<&'a [u8]>,
    authority: Option<&'a [u8]>,
    path: &'a [u8],
    fields: &'a [H2HeaderField],
    effective_host: Option<&'a [u8]>,
    content_length: Option<usize>,
}

impl<'a> H2RequestHeadRef<'a> {
    /// Returns the request method pseudo-header value.
    pub const fn method(&self) -> &'a [u8] {
        self.method
    }

    /// Returns the request scheme pseudo-header value, when present.
    pub const fn scheme(&self) -> Option<&'a [u8]> {
        self.scheme
    }

    /// Returns the request authority pseudo-header value, when present.
    pub const fn authority(&self) -> Option<&'a [u8]> {
        self.authority
    }

    /// Returns the request path pseudo-header value.
    pub const fn path(&self) -> &'a [u8] {
        self.path
    }

    /// Returns regular (non-pseudo) fields in wire order.
    pub const fn fields(&self) -> &'a [H2HeaderField] {
        self.fields
    }

    /// Returns the authority value to synthesize as `host` when no `host` field exists.
    pub const fn effective_host(&self) -> Option<&'a [u8]> {
        self.effective_host
    }

    /// Returns the parsed and validated `content-length`, when present.
    pub const fn content_length(&self) -> Option<usize> {
        self.content_length
    }
}

/// A validated, borrowed HTTP/2 response head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2ResponseHeadRef<'a> {
    status: u16,
    fields: &'a [H2HeaderField],
    content_length: Option<usize>,
}

impl<'a> H2ResponseHeadRef<'a> {
    /// Returns the numeric response status.
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// Returns regular (non-pseudo) fields in wire order.
    pub const fn fields(&self) -> &'a [H2HeaderField] {
        self.fields
    }

    /// Returns the parsed and validated `content-length`, when present.
    pub const fn content_length(&self) -> Option<usize> {
        self.content_length
    }
}

/// Validates and borrows an HTTP/2 request field section without allocating.
pub fn project_h2_request_head(
    fields: &[H2HeaderField],
    limits: HttpLimits,
) -> Result<H2RequestHeadRef<'_>, ServerError> {
    validate_head_fields(fields, H2HeaderValidationRole::Request)?;
    enforce_h2_field_limits(fields, H2HeaderValidationRole::Request, limits)?;

    let regular_start = first_regular_field(fields);
    let pseudo_fields = &fields[..regular_start];
    let regular_fields = &fields[regular_start..];
    let method =
        pseudo_value(pseudo_fields, b":method").expect("validated request fields contain :method");
    let scheme = pseudo_value(pseudo_fields, b":scheme");
    let authority = pseudo_value(pseudo_fields, b":authority");
    let path =
        pseudo_value(pseudo_fields, b":path").expect("validated request fields contain :path");
    let effective_host = if regular_fields.iter().any(|field| field.name == b"host") {
        None
    } else {
        authority
    };

    Ok(H2RequestHeadRef {
        method,
        scheme,
        authority,
        path,
        fields: regular_fields,
        effective_host,
        content_length: content_length_from_raw_headers(regular_fields)?,
    })
}

/// Validates and borrows an HTTP/2 response field section without allocating.
pub fn project_h2_response_head(
    fields: &[H2HeaderField],
    limits: HttpLimits,
) -> Result<H2ResponseHeadRef<'_>, ServerError> {
    validate_head_fields(fields, H2HeaderValidationRole::Response)?;
    enforce_h2_field_limits(fields, H2HeaderValidationRole::Response, limits)?;

    let regular_start = first_regular_field(fields);
    let pseudo_fields = &fields[..regular_start];
    let regular_fields = &fields[regular_start..];
    let status =
        pseudo_value(pseudo_fields, b":status").expect("validated response fields contain :status");
    let status = u16::from(status[0] - b'0') * 100
        + u16::from(status[1] - b'0') * 10
        + u16::from(status[2] - b'0');

    Ok(H2ResponseHeadRef {
        status,
        fields: regular_fields,
        content_length: content_length_from_raw_headers(regular_fields)?,
    })
}

fn first_regular_field(fields: &[H2HeaderField]) -> usize {
    fields
        .iter()
        .position(|field| !field.name.starts_with(b":"))
        .unwrap_or(fields.len())
}

fn validate_head_fields(
    fields: &[H2HeaderField],
    role: H2HeaderValidationRole,
) -> Result<(), ServerError> {
    validate_decoded_header_fields(fields, role).map_err(|error| match error {
        crate::server::H2HeaderValidationError::MalformedMessage => ServerError::InvalidHeader,
        crate::server::H2HeaderValidationError::InvalidContentLength => {
            ServerError::InvalidContentLength
        }
    })
}

fn pseudo_value<'a>(fields: &'a [H2HeaderField], name: &[u8]) -> Option<&'a [u8]> {
    fields
        .iter()
        .find(|field| field.name == name)
        .map(|field| field.value.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HttpErrorKind, hpack_field_size};

    fn field(name: &[u8], value: &[u8]) -> H2HeaderField {
        H2HeaderField::new(name, value)
    }

    fn request_fields() -> Vec<H2HeaderField> {
        vec![
            field(b":method", b"POST"),
            field(b":scheme", b"https"),
            field(b":authority", b"example.test"),
            field(b":path", b"/items"),
            field(b"x-test", b"one"),
            field(b"content-length", b"4"),
        ]
    }

    #[test]
    fn projects_request_head_and_borrows_regular_tail() {
        let fields = request_fields();
        let head = project_h2_request_head(&fields, HttpLimits::new()).unwrap();

        assert_eq!(head.method(), b"POST");
        assert_eq!(head.scheme(), Some(b"https".as_slice()));
        assert_eq!(head.authority(), Some(b"example.test".as_slice()));
        assert_eq!(head.path(), b"/items");
        assert_eq!(head.effective_host(), Some(b"example.test".as_slice()));
        assert_eq!(head.content_length(), Some(4));
        assert_eq!(head.fields().as_ptr(), fields[4..].as_ptr());
        assert_eq!(head.fields(), &fields[4..]);
    }

    #[test]
    fn projects_response_head_and_borrows_regular_tail() {
        let fields = vec![
            field(b":status", b"200"),
            field(b"x-test", b"one"),
            field(b"content-length", b"0"),
        ];
        let head = project_h2_response_head(&fields, HttpLimits::new()).unwrap();

        assert_eq!(head.status(), 200);
        assert_eq!(head.content_length(), Some(0));
        assert_eq!(head.fields().as_ptr(), fields[1..].as_ptr());
        assert_eq!(head.fields(), &fields[1..]);
    }

    #[test]
    fn rejects_missing_and_duplicate_required_pseudo_headers() {
        let missing = vec![
            field(b":method", b"GET"),
            field(b":scheme", b"https"),
            field(b":authority", b"example.test"),
        ];
        assert_eq!(
            project_h2_request_head(&missing, HttpLimits::new()),
            Err(ServerError::InvalidHeader)
        );

        let mut duplicate = request_fields();
        duplicate.insert(1, field(b":method", b"GET"));
        assert_eq!(
            project_h2_request_head(&duplicate, HttpLimits::new()),
            Err(ServerError::InvalidHeader)
        );

        assert_eq!(
            project_h2_response_head(&[], HttpLimits::new()),
            Err(ServerError::InvalidHeader)
        );
    }

    #[test]
    fn rejects_misordered_and_unknown_pseudo_headers() {
        let mut misordered = request_fields();
        misordered.push(field(b":authority", b"late.example"));
        assert_eq!(
            project_h2_request_head(&misordered, HttpLimits::new()),
            Err(ServerError::InvalidHeader)
        );

        let mut unknown = request_fields();
        unknown.insert(1, field(b":unknown", b"value"));
        assert_eq!(
            project_h2_request_head(&unknown, HttpLimits::new()),
            Err(ServerError::InvalidHeader)
        );
    }

    #[test]
    fn rejects_connection_specific_fields() {
        for name in [
            b"connection".as_slice(),
            b"keep-alive",
            b"proxy-connection",
            b"transfer-encoding",
            b"upgrade",
        ] {
            let mut fields = request_fields();
            fields.push(field(name, b"value"));
            assert_eq!(
                project_h2_request_head(&fields, HttpLimits::new()),
                Err(ServerError::InvalidHeader),
                "{name:?}"
            );
        }
    }

    #[test]
    fn accepts_only_trailers_as_te_value() {
        let mut accepted = request_fields();
        accepted.push(field(b"te", b"trailers"));
        project_h2_request_head(&accepted, HttpLimits::new()).unwrap();

        let mut rejected = request_fields();
        rejected.push(field(b"te", b"gzip"));
        assert_eq!(
            project_h2_request_head(&rejected, HttpLimits::new()),
            Err(ServerError::InvalidHeader)
        );
    }

    #[test]
    fn validates_content_length_occurrences() {
        let mut equivalent = request_fields();
        equivalent.pop();
        equivalent.push(field(b"content-length", b"4, 4"));
        equivalent.push(field(b"content-length", b"4"));
        assert_eq!(
            project_h2_request_head(&equivalent, HttpLimits::new())
                .unwrap()
                .content_length(),
            Some(4)
        );

        let mut conflicting = request_fields();
        conflicting.push(field(b"content-length", b"5"));
        assert_eq!(
            project_h2_request_head(&conflicting, HttpLimits::new()),
            Err(ServerError::InvalidContentLength)
        );

        let mut malformed = request_fields();
        malformed.pop();
        malformed.push(field(b"content-length", b"four"));
        assert_eq!(
            project_h2_request_head(&malformed, HttpLimits::new()),
            Err(ServerError::InvalidContentLength)
        );
    }

    #[test]
    fn enforces_header_count_and_aggregate_field_size_limits() {
        let fields = request_fields();
        let count_error = project_h2_request_head(
            &fields,
            HttpLimits::new()
                .set_max_headers(2)
                .set_max_header_bytes(usize::MAX),
        )
        .unwrap_err();
        assert_eq!(count_error.classify().kind(), HttpErrorKind::TooManyHeaders);
        assert_eq!(count_error.classify().limit().unwrap().actual(), Some(3));

        let bytes = fields.iter().fold(0usize, |total, field| {
            total.saturating_add(hpack_field_size(&field.name, &field.value))
        });
        let byte_error = project_h2_request_head(
            &fields,
            HttpLimits::new()
                .set_max_headers(usize::MAX)
                .set_max_header_bytes(bytes - 1),
        )
        .unwrap_err();
        assert_eq!(byte_error.classify().kind(), HttpErrorKind::HeadersTooLarge);
        assert_eq!(byte_error.classify().limit().unwrap().actual(), Some(bytes));
    }

    #[test]
    fn does_not_synthesize_authority_over_an_existing_host() {
        let mut fields = request_fields();
        fields.insert(4, field(b"host", b"explicit.example"));
        let head = project_h2_request_head(&fields, HttpLimits::new()).unwrap();
        assert_eq!(head.effective_host(), None);
    }

    #[test]
    fn maps_supported_http1_versions() {
        assert_eq!(http1_version(0), Ok(Http1Version::Http10));
        assert_eq!(http1_version(1), Ok(Http1Version::Http11));
        assert_eq!(http1_version(2), Err(ServerError::UnsupportedVersion));
    }
}
