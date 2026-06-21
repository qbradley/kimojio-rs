// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HTTP request/response middleware for stackful services.

pub mod add_extension;
pub mod auth;
pub mod cache;
pub mod catch_panic;
pub mod compression;
pub mod cors;
pub mod csrf;
pub mod decompression;
pub mod follow_redirect;
pub mod governor;
pub mod normalize_path;
pub mod propagate_header;
pub mod request_id;
pub mod sensitive_headers;
pub mod session;
pub mod set_header;
pub mod set_status;
pub mod timeout;
pub mod trace;
pub mod validate_request;

pub use add_extension::{AddExtension, AddExtensionLayer};
pub use auth::{Auth, AuthLayer};
pub use cache::{Cache, CacheLayer, CacheStore, MemoryCacheStore};
pub use catch_panic::{CatchPanic, CatchPanicLayer};
pub use compression::{Compression, CompressionLayer, Encoding};
pub use cors::{Cors, CorsLayer};
pub use csrf::{Csrf, CsrfLayer};
pub use decompression::{Decompression, DecompressionLayer};
pub use follow_redirect::{FollowRedirect, FollowRedirectLayer};
pub use governor::{Governor, GovernorLayer};
pub use normalize_path::{NormalizePath, NormalizePathLayer};
pub use propagate_header::{PropagateHeader, PropagateHeaderLayer};
pub use request_id::{RequestId, RequestIdLayer};
pub use sensitive_headers::{SensitiveHeaders, SensitiveHeadersLayer};
pub use session::{MemorySessionStore, Session, SessionLayer, SessionMiddleware, SessionStore};
pub use set_header::{SetHeader, SetHeaderLayer, SetHeaderTarget};
pub use set_status::{SetStatus, SetStatusLayer};
pub use timeout::{HttpTimeout, HttpTimeoutLayer};
pub use trace::{Trace, TraceEvent, TraceLayer, TraceRecord};
pub use validate_request::{ValidateRequest, ValidateRequestLayer};

use http::{Response, StatusCode};
use kimojio_stack_http::Body;

fn empty_response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("valid empty HTTP middleware response")
}
