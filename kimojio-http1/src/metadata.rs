use http::{HeaderMap, HeaderName, HeaderValue, Request, Response};
use kimojio_fsm_http1 as core;

use crate::Error;

pub(crate) fn headers(headers: core::Headers<'_>) -> Result<HeaderMap, Error> {
    let mut result = HeaderMap::try_with_capacity(headers.len()).map_err(|_| Error::Limit)?;
    for header in headers {
        result
            .try_append(
                HeaderName::from_bytes(header.name.as_bytes())
                    .map_err(|_| Error::InvalidMetadata)?,
                HeaderValue::from_bytes(header.value).map_err(|_| Error::InvalidMetadata)?,
            )
            .map_err(|_| Error::Limit)?;
    }
    Ok(result)
}

pub(crate) fn borrowed_headers(headers: &HeaderMap) -> Vec<core::Header<'_>> {
    headers
        .iter()
        .map(|(name, value)| core::Header {
            name: name.as_str(),
            value: value.as_bytes(),
        })
        .collect()
}

pub(crate) fn version(version: http::Version) -> Result<core::Version, Error> {
    match version {
        http::Version::HTTP_10 => Ok(core::Version::Http10),
        http::Version::HTTP_11 => Ok(core::Version::Http11),
        _ => Err(Error::InvalidMetadata),
    }
}

fn http_version(version: core::Version) -> http::Version {
    match version {
        core::Version::Http10 => http::Version::HTTP_10,
        core::Version::Http11 => http::Version::HTTP_11,
    }
}

pub(crate) fn request<T>(head: core::RequestHead<'_>, body: T) -> Result<Request<T>, Error> {
    let mut request = Request::new(body);
    *request.method_mut() = head.method.parse().map_err(|_| Error::InvalidMetadata)?;
    *request.uri_mut() = head.target.parse().map_err(|_| Error::InvalidMetadata)?;
    *request.version_mut() = http_version(head.version);
    *request.headers_mut() = headers(head.headers)?;
    Ok(request)
}

pub(crate) fn response<T>(head: core::ResponseHead<'_>, body: T) -> Result<Response<T>, Error> {
    let mut response = Response::new(body);
    *response.status_mut() = head.status.try_into().map_err(|_| Error::InvalidMetadata)?;
    *response.version_mut() = http_version(head.version);
    *response.headers_mut() = headers(head.headers)?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_values_and_duplicate_fields_survive_conversion() {
        let wire = [
            core::Header {
                name: "X-Value",
                value: b"\xff",
            },
            core::Header {
                name: "X-Value",
                value: b"second",
            },
        ];
        let metadata = headers(&wire).unwrap();
        let values: Vec<_> = metadata.get_all("x-value").iter().collect();
        assert_eq!(values[0].as_bytes(), b"\xff");
        assert_eq!(values[1].as_bytes(), b"second");
        let borrowed = borrowed_headers(&metadata);
        assert_eq!(borrowed.len(), 2);
        assert_eq!(borrowed[0].value, b"\xff");
    }
}
