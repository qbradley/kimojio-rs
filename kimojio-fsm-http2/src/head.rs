use crate::server::{
    H2HeaderValidationRole, H2RawHeaderBlockRef, H2RawHeaderRef, ValidatedHeaderSectionRef,
    ValidatedSection, validate_decoded_header_fields_by,
};
use crate::{H2HeaderField, HttpLimits, ServerError};

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
    let section =
        validate_head_section(fields, H2HeaderValidationRole::Request)?.enforce_limits(limits)?;
    Ok(request_head_from_section(fields, section))
}

pub(crate) struct ValidatedH2RequestHeadRef<'a> {
    method: &'a [u8],
    path: &'a [u8],
    fields: H2RawHeaderBlockRef<'a>,
    effective_host: Option<&'a [u8]>,
    content_length: Option<usize>,
}

impl<'a> ValidatedH2RequestHeadRef<'a> {
    pub(crate) const fn method(&self) -> &'a [u8] {
        self.method
    }

    pub(crate) const fn path(&self) -> &'a [u8] {
        self.path
    }

    pub(crate) const fn fields(&self) -> H2RawHeaderBlockRef<'a> {
        self.fields
    }

    pub(crate) const fn effective_host(&self) -> Option<&'a [u8]> {
        self.effective_host
    }

    pub(crate) const fn content_length(&self) -> Option<usize> {
        self.content_length
    }
}

pub(crate) fn project_h2_request_head_from_validated(
    validated: ValidatedHeaderSectionRef<'_>,
    limits: HttpLimits,
) -> Result<ValidatedH2RequestHeadRef<'_>, ServerError> {
    let (fields, section) = validated.into_parts();
    let section = section.enforce_limits(limits)?;
    if section.role != H2HeaderValidationRole::Request {
        return Err(ServerError::InvalidHeader);
    }
    let request = section.request.ok_or(ServerError::InvalidHeader)?;
    if section.first_regular_index > fields.len()
        || request.method_index >= fields.len()
        || request.path_index >= fields.len()
        || request
            .scheme_index
            .is_some_and(|index| index >= fields.len())
        || request
            .authority_index
            .is_some_and(|index| index >= fields.len())
    {
        return Err(ServerError::InvalidHeader);
    }
    let method = fields
        .get(request.method_index)
        .ok_or(ServerError::InvalidHeader)?
        .value;
    let path = fields
        .get(request.path_index)
        .ok_or(ServerError::InvalidHeader)?
        .value;
    let authority = request
        .authority_index
        .and_then(|index| fields.get(index))
        .map(|field| field.value);
    Ok(ValidatedH2RequestHeadRef {
        method,
        path,
        fields: fields
            .slice_from(section.first_regular_index)
            .ok_or(ServerError::InvalidHeader)?,
        effective_host: if request.has_host { None } else { authority },
        content_length: section.content_length,
    })
}

/// Retains one exact borrowed outbound HTTP/2 request source.
pub(crate) fn prepare_h2_request<'a, F>(
    method: &'a str,
    scheme: &'a str,
    authority: &'a str,
    path: &'a str,
    header_count: usize,
    header_at: F,
    limits: HttpLimits,
) -> Result<PreparedH2Request<'a, F>, ServerError>
where
    F: Fn(usize) -> H2RawHeaderRef<'a> + Copy,
{
    header_count
        .checked_add(4)
        .ok_or(ServerError::HeaderTooLarge {
            limit: limits.max_header_bytes(),
            actual: usize::MAX,
        })?;
    Ok(PreparedH2Request {
        method,
        scheme,
        authority,
        path,
        header_count,
        header_at,
        limits,
    })
}

pub(crate) struct PreparedH2Request<'a, F> {
    method: &'a str,
    scheme: &'a str,
    authority: &'a str,
    path: &'a str,
    header_count: usize,
    header_at: F,
    limits: HttpLimits,
}

impl<'a, F> PreparedH2Request<'a, F>
where
    F: Fn(usize) -> H2RawHeaderRef<'a> + Copy,
{
    pub(crate) fn into_parts(self) -> (&'a str, &'a str, &'a str, &'a str, usize, F, HttpLimits) {
        (
            self.method,
            self.scheme,
            self.authority,
            self.path,
            self.header_count,
            self.header_at,
            self.limits,
        )
    }
}

/// Validates and borrows an HTTP/2 response field section without allocating.
pub fn project_h2_response_head(
    fields: &[H2HeaderField],
    limits: HttpLimits,
) -> Result<H2ResponseHeadRef<'_>, ServerError> {
    let section =
        validate_head_section(fields, H2HeaderValidationRole::Response)?.enforce_limits(limits)?;
    Ok(response_head_from_section(fields, section))
}

pub(crate) struct ValidatedH2ResponseHeadRef<'a> {
    status: u16,
    fields: H2RawHeaderBlockRef<'a>,
    content_length: Option<usize>,
}

impl<'a> ValidatedH2ResponseHeadRef<'a> {
    pub(crate) const fn status(&self) -> u16 {
        self.status
    }

    pub(crate) const fn fields(&self) -> H2RawHeaderBlockRef<'a> {
        self.fields
    }

    pub(crate) const fn content_length(&self) -> Option<usize> {
        self.content_length
    }
}

pub(crate) fn project_h2_response_head_from_validated(
    validated: ValidatedHeaderSectionRef<'_>,
    limits: HttpLimits,
) -> Result<ValidatedH2ResponseHeadRef<'_>, ServerError> {
    let (fields, section) = validated.into_parts();
    let section = section.enforce_limits(limits)?;
    if section.role != H2HeaderValidationRole::Response
        || section.response_status.is_none()
        || section.first_regular_index > fields.len()
    {
        return Err(ServerError::InvalidHeader);
    }
    Ok(ValidatedH2ResponseHeadRef {
        status: section
            .response_status
            .expect("validated response section contains :status"),
        fields: fields
            .slice_from(section.first_regular_index)
            .ok_or(ServerError::InvalidHeader)?,
        content_length: section.content_length,
    })
}

#[cfg(test)]
fn first_regular_field(fields: &[H2HeaderField]) -> usize {
    fields
        .iter()
        .position(|field| !field.name.starts_with(b":"))
        .unwrap_or(fields.len())
}

fn validate_head_section(
    fields: &[H2HeaderField],
    role: H2HeaderValidationRole,
) -> Result<ValidatedSection, ServerError> {
    validate_head_section_by(fields.len(), |index| fields[index].as_ref(), role)
}

fn validate_head_section_by<'a>(
    field_count: usize,
    field_at: impl Fn(usize) -> H2RawHeaderRef<'a>,
    role: H2HeaderValidationRole,
) -> Result<ValidatedSection, ServerError> {
    validate_decoded_header_fields_by(field_count, field_at, role).map_err(|error| match error {
        crate::server::H2HeaderValidationError::MalformedMessage => ServerError::InvalidHeader,
        crate::server::H2HeaderValidationError::InvalidContentLength => {
            ServerError::InvalidContentLength
        }
    })
}

fn request_head_from_section<'a>(
    fields: &'a [H2HeaderField],
    section: ValidatedSection,
) -> H2RequestHeadRef<'a> {
    let request = section
        .request
        .expect("validated request section contains request facts");
    let authority = request
        .authority_index
        .map(|index| fields[index].value.as_slice());
    H2RequestHeadRef {
        method: &fields[request.method_index].value,
        scheme: request
            .scheme_index
            .map(|index| fields[index].value.as_slice()),
        authority,
        path: &fields[request.path_index].value,
        fields: &fields[section.first_regular_index..],
        effective_host: if request.has_host { None } else { authority },
        content_length: section.content_length,
    }
}

fn response_head_from_section<'a>(
    fields: &'a [H2HeaderField],
    section: ValidatedSection,
) -> H2ResponseHeadRef<'a> {
    H2ResponseHeadRef {
        status: section
            .response_status
            .expect("validated response section contains :status"),
        fields: &fields[section.first_regular_index..],
        content_length: section.content_length,
    }
}
