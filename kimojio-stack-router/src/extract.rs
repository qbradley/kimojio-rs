// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Request extraction helpers.

use std::any::type_name;
use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, Method, Request, Uri};
use kimojio_stack_http::Body;

use crate::{Rejection, RejectionKind};

#[derive(Clone)]
pub(crate) struct SharedState(pub Arc<dyn std::any::Any + Send + Sync>);

/// Route path parameters captured by the router.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PathParams {
    values: BTreeMap<String, String>,
}

impl PathParams {
    /// Creates path params from key-value pairs.
    pub fn new(values: BTreeMap<String, String>) -> Self {
        Self { values }
    }

    /// Gets a path parameter by name.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// Returns all captured path parameters.
    pub fn as_map(&self) -> &BTreeMap<String, String> {
        &self.values
    }
}

/// Helper for constructing [`PathParams`] in tests and adapters.
pub fn path_params(
    values: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
) -> PathParams {
    PathParams::new(
        values
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect(),
    )
}

/// Parsed query string parameters.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct QueryParams {
    values: BTreeMap<String, String>,
}

impl QueryParams {
    /// Parses query parameters from a URI.
    pub fn from_uri(uri: &Uri) -> Self {
        let mut values = BTreeMap::new();
        if let Some(query) = uri.query() {
            for pair in query.split('&').filter(|pair| !pair.is_empty()) {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                values.insert(percent_decode(key), percent_decode(value));
            }
        }
        Self { values }
    }

    /// Gets a query parameter by name.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// Returns all parsed query parameters.
    pub fn as_map(&self) -> &BTreeMap<String, String> {
        &self.values
    }
}

/// Shared application state extractor.
#[derive(Clone, Debug)]
pub struct State<T>(pub Arc<T>);

/// Request extension extractor.
#[derive(Clone, Debug)]
pub struct Extension<T>(pub T);

/// Complete request body bytes extractor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BodyBytes(pub Bytes);

/// Extracts a typed value from a request.
pub trait FromRequest<Cx>: Sized {
    /// Attempts extraction from `request`.
    fn from_request(cx: &Cx, request: &mut Request<Body>) -> Result<Self, Rejection>;
}

impl<Cx> FromRequest<Cx> for Request<Body> {
    fn from_request(_cx: &Cx, request: &mut Request<Body>) -> Result<Self, Rejection> {
        let mut replacement = Request::new(Body::empty());
        *replacement.method_mut() = request.method().clone();
        *replacement.uri_mut() = request.uri().clone();
        *replacement.version_mut() = request.version();
        *replacement.headers_mut() = request.headers().clone();
        *replacement.extensions_mut() = std::mem::take(request.extensions_mut());
        std::mem::swap(request.body_mut(), replacement.body_mut());
        Ok(replacement)
    }
}

impl<Cx> FromRequest<Cx> for Method {
    fn from_request(_cx: &Cx, request: &mut Request<Body>) -> Result<Self, Rejection> {
        Ok(request.method().clone())
    }
}

impl<Cx> FromRequest<Cx> for Uri {
    fn from_request(_cx: &Cx, request: &mut Request<Body>) -> Result<Self, Rejection> {
        Ok(request.uri().clone())
    }
}

impl<Cx> FromRequest<Cx> for HeaderMap {
    fn from_request(_cx: &Cx, request: &mut Request<Body>) -> Result<Self, Rejection> {
        Ok(request.headers().clone())
    }
}

impl<Cx> FromRequest<Cx> for PathParams {
    fn from_request(_cx: &Cx, request: &mut Request<Body>) -> Result<Self, Rejection> {
        request
            .extensions()
            .get::<PathParams>()
            .cloned()
            .ok_or_else(|| Rejection::new(RejectionKind::Path, "missing path parameters"))
    }
}

impl<Cx> FromRequest<Cx> for QueryParams {
    fn from_request(_cx: &Cx, request: &mut Request<Body>) -> Result<Self, Rejection> {
        Ok(QueryParams::from_uri(request.uri()))
    }
}

impl<Cx> FromRequest<Cx> for BodyBytes {
    fn from_request(_cx: &Cx, request: &mut Request<Body>) -> Result<Self, Rejection> {
        Ok(Self(request.body().clone().into_bytes()))
    }
}

impl<Cx, T> FromRequest<Cx> for State<T>
where
    T: Send + Sync + 'static,
{
    fn from_request(_cx: &Cx, request: &mut Request<Body>) -> Result<Self, Rejection> {
        let state = request
            .extensions()
            .get::<SharedState>()
            .cloned()
            .ok_or_else(|| {
                Rejection::new(
                    RejectionKind::State,
                    format!("missing state {}", type_name::<T>()),
                )
            })?;
        state.0.downcast::<T>().map(Self).map_err(|_| {
            Rejection::new(
                RejectionKind::State,
                format!("state type mismatch {}", type_name::<T>()),
            )
        })
    }
}

impl<Cx, T> FromRequest<Cx> for Extension<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn from_request(_cx: &Cx, request: &mut Request<Body>) -> Result<Self, Rejection> {
        request
            .extensions()
            .get::<T>()
            .cloned()
            .map(Self)
            .ok_or_else(|| {
                Rejection::new(
                    RejectionKind::Extension,
                    format!("missing extension {}", type_name::<T>()),
                )
            })
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hi = hex(bytes[index + 1]);
                let lo = hex(bytes[index + 2]);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    output.push((hi << 4) | lo);
                    index += 3;
                } else {
                    output.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
