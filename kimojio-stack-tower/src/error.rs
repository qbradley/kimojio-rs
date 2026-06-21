// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Structured error types for stackful services and middleware.

use std::error::Error;
use std::fmt;

/// Boxed error used by type-erased services and middleware.
pub type BoxError = Box<dyn Error + Send + Sync + 'static>;

/// Common error categories for stackful services and middleware.
#[derive(Debug)]
pub enum ServiceError {
    /// A service is not ready to accept a request yet.
    NotReady,
    /// A service or middleware layer is overloaded and rejected work.
    Overloaded,
    /// A service or middleware layer exceeded a configured deadline.
    Timeout,
    /// In-flight work was canceled.
    Canceled,
    /// The request was invalid for this service.
    InvalidRequest(&'static str),
    /// An inner service failed.
    Inner(BoxError),
    /// A named middleware layer failed while calling an inner service.
    Layer {
        /// Human-readable layer name.
        layer: &'static str,
        /// Source error from the wrapped service or middleware.
        source: BoxError,
    },
}

impl ServiceError {
    /// Wraps `source` with a named layer context.
    pub fn layer(layer: &'static str, source: impl Into<BoxError>) -> Self {
        Self::Layer {
            layer,
            source: source.into(),
        }
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotReady => f.write_str("service is not ready"),
            Self::Overloaded => f.write_str("service is overloaded"),
            Self::Timeout => f.write_str("service request timed out"),
            Self::Canceled => f.write_str("service request was canceled"),
            Self::InvalidRequest(message) => write!(f, "invalid request: {message}"),
            Self::Inner(error) => write!(f, "inner service failed: {error}"),
            Self::Layer { layer, source } => write!(f, "service layer {layer} failed: {source}"),
        }
    }
}

impl Error for ServiceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Inner(error) => Some(error.as_ref()),
            Self::Layer { source, .. } => Some(source.as_ref()),
            Self::NotReady
            | Self::Overloaded
            | Self::Timeout
            | Self::Canceled
            | Self::InvalidRequest(_) => None,
        }
    }
}
