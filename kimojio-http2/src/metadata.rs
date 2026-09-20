use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, Uri};
use kimojio_fsm_http2::{H2HeaderField, H2RawHeaderRef, Head};

use crate::{Error, IncomingBody, OutgoingBody};

pub(crate) fn incoming_request(
    head: Head<'_>,
    body: IncomingBody,
) -> Result<Request<IncomingBody>, Error> {
    let get = |name: &[u8]| {
        head.fields()
            .find(|field| field.name == name)
            .map(|field| field.value)
    };
    let method = Method::from_bytes(get(b":method").ok_or(Error::InvalidMetadata)?)
        .map_err(|_| Error::InvalidMetadata)?;
    let headers = headers(head)?;
    let text = |value| std::str::from_utf8(value).map_err(|_| Error::InvalidMetadata);
    let authority =
        get(b":authority").or_else(|| headers.get(http::header::HOST).map(HeaderValue::as_bytes));
    let uri = if method == Method::CONNECT {
        Uri::builder()
            .authority(text(authority.ok_or(Error::InvalidMetadata)?)?)
            .build()
            .map_err(|_| Error::InvalidMetadata)?
    } else {
        let path = text(get(b":path").ok_or(Error::InvalidMetadata)?)?;
        if let Some(authority) = authority {
            Uri::builder()
                .scheme(text(get(b":scheme").ok_or(Error::InvalidMetadata)?)?)
                .authority(text(authority)?)
                .path_and_query(path)
                .build()
                .map_err(|_| Error::InvalidMetadata)?
        } else {
            path.parse().map_err(|_| Error::InvalidMetadata)?
        }
    };
    let mut request = Request::new(body);
    *request.method_mut() = method;
    *request.uri_mut() = uri;
    *request.version_mut() = http::Version::HTTP_2;
    *request.headers_mut() = headers;
    Ok(request)
}

pub(crate) fn response(
    response: Response<OutgoingBody>,
) -> Result<(Vec<H2HeaderField>, OutgoingBody), Error> {
    let (parts, body) = response.into_parts();
    if parts.status.is_informational() {
        return Err(Error::InvalidMetadata);
    }
    let mut fields = trailers(parts.headers);
    fields.insert(0, field(b":status", parts.status.as_str().as_bytes()));
    Ok((fields, body))
}

pub(crate) fn informational(response: Response<()>) -> Result<Vec<H2HeaderField>, Error> {
    if !response.status().is_informational()
        || response.status() == http::StatusCode::SWITCHING_PROTOCOLS
    {
        return Err(Error::InvalidMetadata);
    }
    let (parts, ()) = response.into_parts();
    let mut fields = trailers(parts.headers);
    fields.insert(0, field(b":status", parts.status.as_str().as_bytes()));
    Ok(fields)
}

pub(crate) fn request(
    request: Request<OutgoingBody>,
) -> Result<(Vec<H2HeaderField>, OutgoingBody), Error> {
    let (parts, body) = request.into_parts();
    let authority = parts
        .uri
        .authority()
        .map(|a| a.as_str())
        .or_else(|| {
            parts
                .headers
                .get(http::header::HOST)
                .and_then(|h| h.to_str().ok())
        })
        .ok_or(Error::InvalidMetadata)?;
    let mut fields = Vec::with_capacity(parts.headers.len() + 4);
    fields.push(field(b":method", parts.method.as_str().as_bytes()));
    fields.push(field(b":authority", authority.as_bytes()));
    if parts.method != Method::CONNECT {
        let scheme = parts.uri.scheme_str().ok_or(Error::InvalidMetadata)?;
        fields.push(field(b":scheme", scheme.as_bytes()));
        fields.push(field(
            b":path",
            parts
                .uri
                .path_and_query()
                .map_or("/", |p| p.as_str())
                .as_bytes(),
        ));
    }
    for (name, value) in &parts.headers {
        let mut field = field(name.as_str().as_bytes(), value.as_bytes());
        field.sensitive = value.is_sensitive();
        fields.push(field);
    }
    Ok((fields, body))
}

fn field(name: &[u8], value: &[u8]) -> H2HeaderField {
    H2HeaderField {
        name: name.to_vec(),
        value: value.to_vec(),
        sensitive: false,
    }
}

pub(crate) fn trailers(headers: HeaderMap) -> Vec<H2HeaderField> {
    headers
        .iter()
        .map(|(name, value)| {
            let mut field = field(name.as_str().as_bytes(), value.as_bytes());
            field.sensitive = value.is_sensitive();
            field
        })
        .collect()
}

pub(crate) fn borrowed(fields: &[H2HeaderField]) -> Vec<H2RawHeaderRef<'_>> {
    fields.iter().map(H2HeaderField::as_ref).collect()
}

pub(crate) fn storage(fields: &Vec<H2HeaderField>) -> usize {
    fields.capacity() * size_of::<H2HeaderField>()
        + fields
            .iter()
            .map(|field| field.name.capacity() + field.value.capacity())
            .sum::<usize>()
}

pub(crate) fn headers(head: Head<'_>) -> Result<HeaderMap, Error> {
    let mut headers = HeaderMap::try_with_capacity(head.len()).map_err(|_| Error::Limit)?;
    for field in head.fields().filter(|field| !field.name.starts_with(b":")) {
        let mut value = HeaderValue::from_bytes(field.value).map_err(|_| Error::InvalidMetadata)?;
        value.set_sensitive(field.sensitive);
        headers
            .try_append(
                HeaderName::from_bytes(field.name).map_err(|_| Error::InvalidMetadata)?,
                value,
            )
            .map_err(|_| Error::Limit)?;
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_fields_preserve_duplicates_and_sensitivity() {
        let mut request = Request::builder()
            .uri("https://example.test/path?query")
            .body(OutgoingBody::empty())
            .unwrap();
        request
            .headers_mut()
            .append("x-value", "first".parse().unwrap());
        request
            .headers_mut()
            .append("x-value", "second".parse().unwrap());
        let mut secret = HeaderValue::from_static("secret");
        secret.set_sensitive(true);
        request.headers_mut().insert("authorization", secret);
        let (fields, _) = super::request(request).unwrap();
        let values: Vec<_> = fields
            .iter()
            .filter(|field| field.name == b"x-value")
            .map(|field| field.value.as_slice())
            .collect();
        assert_eq!(values, [b"first".as_slice(), b"second".as_slice()]);
        assert!(
            fields
                .iter()
                .find(|field| field.name == b"authorization")
                .unwrap()
                .sensitive
        );
        assert!(
            fields
                .iter()
                .any(|field| field.name == b":path" && field.value == b"/path?query")
        );
        assert!(storage(&fields) >= fields.len() * size_of::<H2HeaderField>());
    }

    #[test]
    fn connect_uses_authority_without_scheme_or_path() {
        let request = Request::builder()
            .method("CONNECT")
            .uri("example.test:443")
            .body(OutgoingBody::empty())
            .unwrap();
        let (fields, _) = super::request(request).unwrap();
        assert_eq!(fields.len(), 2);
        assert!(
            fields
                .iter()
                .any(|field| field.name == b":authority" && field.value == b"example.test:443")
        );
        assert!(super::request(Request::new(OutgoingBody::empty())).is_err());
    }
}
