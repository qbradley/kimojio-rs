// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Copy-from-source request helpers.
//!
//! Copy operations are represented as a destination object plus a caller-supplied
//! [`SignedSource`]. The source may carry an authorization header as well as a
//! signed URL; debug output in the auth module redacts both.

use crate::{
    AttemptError, Conditions, Error, ErrorKind, ObjectRef, OperationClass, RequestParts,
    ResponseParts, SignedSource, Transport, properties::object_uri,
};

/// Copy acceptance information captured from response metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopyInfo {
    /// Service copy ID, when present.
    pub copy_id: Option<String>,
    /// Service copy status, such as `pending` or `success`.
    pub status: Option<String>,
    /// Request ID from response metadata or diagnostics.
    pub request_id: Option<String>,
}

/// Client helper for copy-from-source operations.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CopyClient;

impl CopyClient {
    /// Starts a copy operation and requires the service to accept it.
    pub fn copy_from_source<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        destination: &ObjectRef,
        source: &SignedSource,
        conditions: Option<&Conditions>,
    ) -> Result<CopyInfo, AttemptError> {
        let response = transport.execute(
            cx,
            &copy_from_source_request(destination, source, conditions),
        )?;
        if response.status != 202 {
            return Err(AttemptError {
                error: Error::new(
                    ErrorKind::UnhandledService,
                    format!("copy expected HTTP 202, received {}", response.status),
                ),
                diagnostics: response.diagnostics,
            });
        }
        let info = copy_info_from_response(&response);
        require_accepted_copy(&info).map_err(|error| AttemptError {
            error,
            diagnostics: response.diagnostics,
        })?;
        Ok(info)
    }
}

/// Builds a copy-from-source request.
pub fn copy_from_source_request(
    destination: &ObjectRef,
    source: &SignedSource,
    conditions: Option<&Conditions>,
) -> RequestParts {
    let mut request = RequestParts::new(OperationClass::Copy, "PUT", object_uri(destination));
    request.metadata.insert("content-length", "0");
    request
        .metadata
        .insert("x-ms-copy-source", source.reference());
    if let Some(authorization) = source.authorization() {
        request
            .metadata
            .insert("x-ms-copy-source-authorization", authorization);
    }
    if let Some(conditions) = conditions {
        conditions.apply(&mut request);
    }
    request
}

/// Extracts copy information from a response.
pub fn copy_info_from_response(response: &ResponseParts) -> CopyInfo {
    CopyInfo {
        copy_id: response.metadata.get("x-ms-copy-id").map(str::to_owned),
        status: response.metadata.get("x-ms-copy-status").map(str::to_owned),
        request_id: response
            .metadata
            .get("x-ms-request-id")
            .map(str::to_owned)
            .or_else(|| {
                response
                    .diagnostics
                    .attempts()
                    .first()
                    .and_then(|attempt| attempt.request_id.clone())
            }),
    }
}

/// Converts an attempt error into an inspectable copy error and best-effort info.
pub fn copy_error_with_diagnostics(error: AttemptError) -> (Error, CopyInfo) {
    let info = CopyInfo {
        copy_id: None,
        status: error.error.service_code().map(str::to_owned),
        request_id: error.error.request_id().map(str::to_owned).or_else(|| {
            error
                .diagnostics
                .attempts()
                .first()
                .and_then(|attempt| attempt.request_id.clone())
        }),
    };
    (error.error, info)
}

/// Requires a copy status that means the operation was accepted.
pub fn require_accepted_copy(info: &CopyInfo) -> Result<(), Error> {
    match info.status.as_deref() {
        Some("success" | "pending") => Ok(()),
        Some(_) => Err(Error::new(
            ErrorKind::UnhandledService,
            "copy was not accepted",
        )),
        None => Err(Error::new(ErrorKind::Corruption, "copy status missing")),
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        AccountId, AttemptDiagnostics, ContainerName, Diagnostics, MetadataMap, ObjectKind,
        ObjectName,
    };

    use super::*;

    #[derive(Clone)]
    struct FakeTransport {
        response: ResponseParts,
    }

    impl Transport for FakeTransport {
        fn execute(
            &mut self,
            _cx: &kimojio_stack::RuntimeContext<'_>,
            _request: &RequestParts,
        ) -> Result<ResponseParts, AttemptError> {
            Ok(self.response.clone())
        }
    }

    fn object() -> ObjectRef {
        ObjectRef {
            account: AccountId::new("acct"),
            container: ContainerName::new("container"),
            name: ObjectName::new("copy-dest"),
            kind: ObjectKind::Backup,
        }
    }

    #[test]
    fn copy_request_preserves_source_authorization_and_conditions() {
        let source = SignedSource::new("https://source/object").with_authorization("Bearer token");
        let request =
            copy_from_source_request(&object(), &source, Some(&Conditions::if_match("e")));

        assert_eq!(request.method, "PUT");
        assert_eq!(request.metadata.get("content-length"), Some("0"));
        assert_eq!(
            request.metadata.get("x-ms-copy-source"),
            Some("https://source/object")
        );
        assert_eq!(
            request.metadata.get("x-ms-copy-source-authorization"),
            Some("Bearer token")
        );
        assert_eq!(request.metadata.get("if-match"), Some("e"));
    }

    #[test]
    fn copy_response_and_error_diagnostics_are_inspectable() {
        let mut metadata = MetadataMap::new();
        metadata.insert("x-ms-copy-id", "copy-1");
        metadata.insert("x-ms-copy-status", "pending");
        metadata.insert("x-ms-request-id", "req-1");
        let response = ResponseParts {
            status: 202,
            metadata,
            diagnostics: Diagnostics::new(OperationClass::Copy),
        };
        let info = copy_info_from_response(&response);
        assert_eq!(info.copy_id.as_deref(), Some("copy-1"));
        assert!(require_accepted_copy(&info).is_ok());

        let mut diagnostics = Diagnostics::new(OperationClass::Copy);
        diagnostics.push_attempt(AttemptDiagnostics {
            attempt: 1,
            status: Some(500),
            service_code: Some("copy-failed".into()),
            request_id: Some("req-err".into()),
            elapsed: None,
            retriable: false,
        });
        let (_error, info) = copy_error_with_diagnostics(AttemptError {
            error: Error::new(ErrorKind::Unavailable, "copy failed"),
            diagnostics,
        });
        assert_eq!(info.request_id.as_deref(), Some("req-err"));
    }

    #[test]
    fn copy_client_rejects_missing_or_failed_copy_status() {
        let source = SignedSource::new("https://source/object");
        let mut failed_metadata = MetadataMap::new();
        failed_metadata.insert("x-ms-copy-status", "failed");
        let mut failed = FakeTransport {
            response: ResponseParts {
                status: 202,
                metadata: failed_metadata,
                diagnostics: Diagnostics::new(OperationClass::Copy),
            },
        };
        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| CopyClient.copy_from_source(cx, &mut failed, &object(), &source, None))
            .unwrap_err();
        assert_eq!(error.error.kind(), ErrorKind::UnhandledService);

        let mut ok_metadata = MetadataMap::new();
        ok_metadata.insert("x-ms-copy-status", "pending");
        let mut ok = FakeTransport {
            response: ResponseParts {
                status: 202,
                metadata: ok_metadata,
                diagnostics: Diagnostics::new(OperationClass::Copy),
            },
        };
        assert!(
            kimojio_stack::Runtime::new()
                .block_on(|cx| CopyClient.copy_from_source(cx, &mut ok, &object(), &source, None))
                .is_ok()
        );

        let mut wrong_status_metadata = MetadataMap::new();
        wrong_status_metadata.insert("x-ms-copy-status", "pending");
        let mut wrong_status = FakeTransport {
            response: ResponseParts {
                status: 200,
                metadata: wrong_status_metadata,
                diagnostics: Diagnostics::new(OperationClass::Copy),
            },
        };
        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| {
                CopyClient.copy_from_source(cx, &mut wrong_status, &object(), &source, None)
            })
            .unwrap_err();
        assert_eq!(error.error.kind(), ErrorKind::UnhandledService);
    }
}
