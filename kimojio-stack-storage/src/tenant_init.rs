// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tenant/container initialization helpers.
//!
//! Initialization is designed to be explicit and idempotence-aware. The
//! initializer creates target containers, ensures required page objects exist
//! with expected sizes, and finally writes metadata with an initialized marker.
//! A caller-owned [`TenantInitState`] prevents accidental repeated initialization
//! from the same process.

use crate::{
    AccountId, AttemptError, ContainerClient, ContainerName, Diagnostics, Error, ErrorKind,
    MetadataMap, ObjectRef, OperationClass, PageClient, StorageRuntime, Transport,
    properties::object_properties_request, properties::set_object_metadata_request,
};

/// Metadata key used to mark initialization completion.
pub const INITIALIZED_MARKER: &str = "initialized";

/// Container that should exist before tenant startup completes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerTarget {
    /// Account that owns the container.
    pub account: AccountId,
    /// Container name.
    pub container: ContainerName,
}

/// Page object that should exist with a specific length.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageObjectInit {
    /// Page object reference.
    pub object: ObjectRef,
    /// Required object length in bytes.
    pub content_len: u64,
}

/// Complete initialization plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenantInitPlan {
    /// Containers to create if absent.
    pub containers: Vec<ContainerTarget>,
    /// Metadata object that receives the initialized marker.
    pub metadata_object: ObjectRef,
    /// Metadata to write with the initialized marker.
    pub metadata: MetadataMap,
    /// Page objects to create or verify.
    pub page_objects: Vec<PageObjectInit>,
}

/// Caller-owned initialization state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TenantInitState {
    initialized: bool,
}

impl TenantInitState {
    /// Returns whether initialization completed in this state object.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}

/// Client helper that executes a [`TenantInitPlan`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TenantInitializer;

impl TenantInitializer {
    /// Runs tenant initialization.
    ///
    /// The final initialized marker is written with an ETag condition when a
    /// metadata object already exists, reducing the chance of racing initializers
    /// silently overwriting each other.
    pub fn initialize<'cx, R, T>(
        self,
        cx: &'cx R::Context<'cx>,
        transport: &mut T,
        plan: &TenantInitPlan,
        state: &mut TenantInitState,
    ) -> Result<(), AttemptError>
    where
        R: StorageRuntime,
        T: Transport<R>,
    {
        if state.initialized {
            return Err(attempt_error(Error::new(
                ErrorKind::AlreadyExists,
                "tenant has already been initialized",
            )));
        }
        let mut final_marker_condition = None;
        let metadata_exists = match transport.execute(
            cx,
            &object_properties_request(&plan.metadata_object, None, None),
        ) {
            Ok(response)
                if response.metadata.get("x-ms-meta-initialized") == Some("true")
                    || response.metadata.get(INITIALIZED_MARKER) == Some("true") =>
            {
                state.initialized = true;
                return Err(attempt_error(Error::new(
                    ErrorKind::AlreadyExists,
                    "tenant initialized marker already exists",
                )));
            }
            Ok(response) => {
                let etag = response.metadata.get("etag").ok_or_else(|| {
                    attempt_error(Error::new(
                        ErrorKind::Corruption,
                        "metadata object properties missing ETag",
                    ))
                })?;
                final_marker_condition = Some(crate::Conditions::if_match(etag));
                true
            }
            Err(error) if error.error.kind() == ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };

        for target in &plan.containers {
            ContainerClient.create(cx, transport, &target.account, &target.container)?;
        }
        if !metadata_exists {
            PageClient.create(
                cx,
                transport,
                &plan.metadata_object,
                0,
                &MetadataMap::new(),
                None,
            )?;
            let properties = transport.execute(
                cx,
                &object_properties_request(&plan.metadata_object, None, None),
            )?;
            let etag = properties.metadata.get("etag").ok_or_else(|| {
                attempt_error(Error::new(
                    ErrorKind::Corruption,
                    "created metadata object properties missing ETag",
                ))
            })?;
            final_marker_condition = Some(crate::Conditions::if_match(etag));
        }
        for page in &plan.page_objects {
            ensure_page_object(cx, transport, page)?;
        }
        let mut metadata = plan.metadata.clone();
        metadata.insert(INITIALIZED_MARKER, "true");
        let metadata_request = set_object_metadata_request(
            &plan.metadata_object,
            &metadata,
            final_marker_condition.as_ref(),
            None,
        );
        transport.execute(cx, &metadata_request)?;

        state.initialized = true;
        Ok(())
    }
}

fn ensure_page_object<'cx, R, T>(
    cx: &'cx R::Context<'cx>,
    transport: &mut T,
    page: &PageObjectInit,
) -> Result<(), AttemptError>
where
    R: StorageRuntime,
    T: Transport<R>,
{
    match PageClient.create(
        cx,
        transport,
        &page.object,
        page.content_len,
        &MetadataMap::new(),
        None,
    ) {
        Ok(_) => Ok(()),
        Err(error)
            if matches!(
                error.error.kind(),
                ErrorKind::AlreadyExists | ErrorKind::ConditionNotMet
            ) =>
        {
            let properties = PageClient.get_properties(cx, transport, &page.object)?;
            if properties.content_len == page.content_len {
                Ok(())
            } else {
                Err(attempt_error(Error::new(
                    ErrorKind::AlreadyExists,
                    format!(
                        "existing page object size {} does not match expected {}",
                        properties.content_len, page.content_len
                    ),
                )))
            }
        }
        Err(error) => Err(error),
    }
}

fn attempt_error(error: Error) -> AttemptError {
    AttemptError {
        error,
        diagnostics: Diagnostics::new(OperationClass::Metadata),
    }
}

#[cfg(test)]
mod tests {
    use crate::{ObjectKind, ObjectName, RequestParts, ResponseParts};

    use super::*;

    #[derive(Default)]
    struct RecordingTransport {
        requests: Vec<RequestParts>,
        responses: Vec<Result<ResponseParts, AttemptError>>,
    }

    impl Transport for RecordingTransport {
        fn execute(
            &mut self,
            _cx: &kimojio_stack::RuntimeContext<'_>,
            request: &RequestParts,
        ) -> Result<ResponseParts, AttemptError> {
            self.requests.push(request.clone());
            if !self.responses.is_empty() {
                return self.responses.remove(0);
            }
            let mut metadata = MetadataMap::new();
            metadata.insert("etag", "default-etag");
            Ok(ResponseParts {
                status: 200,
                metadata,
                diagnostics: Diagnostics::new(request.operation),
            })
        }
    }

    fn object(name: &str, kind: ObjectKind) -> ObjectRef {
        ObjectRef {
            account: AccountId::new("acct"),
            container: ContainerName::new("container"),
            name: ObjectName::new(name),
            kind,
        }
    }

    fn etag_metadata(etag: &str) -> MetadataMap {
        let mut metadata = MetadataMap::new();
        metadata.insert("etag", etag);
        metadata
    }

    #[test]
    fn tenant_initializer_creates_containers_metadata_and_pages_once() {
        let mut metadata = MetadataMap::new();
        metadata.insert("initialized", "true");
        let plan = TenantInitPlan {
            containers: vec![ContainerTarget {
                account: AccountId::new("acct"),
                container: ContainerName::new("container"),
            }],
            metadata_object: object("metadata/root", ObjectKind::Metadata),
            metadata,
            page_objects: vec![PageObjectInit {
                object: object("data/page", ObjectKind::Data),
                content_len: 512,
            }],
        };
        let mut state = TenantInitState::default();
        let mut transport = RecordingTransport {
            responses: vec![Err(attempt_error(Error::new(
                ErrorKind::NotFound,
                "missing",
            )))],
            ..RecordingTransport::default()
        };

        kimojio_stack::Runtime::new()
            .block_on(|cx| TenantInitializer.initialize(cx, &mut transport, &plan, &mut state))
            .unwrap();

        assert!(state.is_initialized());
        assert_eq!(transport.requests.len(), 6);
        assert_eq!(transport.requests[0].method, "HEAD");
        assert_eq!(
            transport.requests[1].metadata.get("if-none-match"),
            Some("*")
        );
        assert_eq!(
            transport.requests[2]
                .metadata
                .get("x-ms-blob-content-length"),
            Some("0")
        );
        assert_eq!(transport.requests[3].method, "HEAD");
        assert_eq!(
            transport.requests[4]
                .metadata
                .get("x-ms-blob-content-length"),
            Some("512")
        );
        assert_eq!(
            transport.requests[5].metadata.get("x-ms-meta-initialized"),
            Some("true")
        );

        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| TenantInitializer.initialize(cx, &mut transport, &plan, &mut state))
            .unwrap_err();
        assert_eq!(error.error.kind(), ErrorKind::AlreadyExists);

        let mut initialized = MetadataMap::new();
        initialized.insert("x-ms-meta-initialized", "true");
        let mut fresh_transport = RecordingTransport {
            responses: vec![Ok(ResponseParts {
                status: 200,
                metadata: initialized,
                diagnostics: Diagnostics::new(OperationClass::Metadata),
            })],
            ..RecordingTransport::default()
        };
        let mut fresh_state = TenantInitState::default();
        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| {
                TenantInitializer.initialize(cx, &mut fresh_transport, &plan, &mut fresh_state)
            })
            .unwrap_err();
        assert_eq!(error.error.kind(), ErrorKind::AlreadyExists);
        assert!(fresh_state.is_initialized());
    }

    #[test]
    fn tenant_initializer_does_not_mark_initialized_when_page_creation_fails() {
        let plan = TenantInitPlan {
            containers: vec![ContainerTarget {
                account: AccountId::new("acct"),
                container: ContainerName::new("container"),
            }],
            metadata_object: object("metadata/root", ObjectKind::Metadata),
            metadata: MetadataMap::new(),
            page_objects: vec![PageObjectInit {
                object: object("data/page", ObjectKind::Data),
                content_len: 512,
            }],
        };
        let mut transport = RecordingTransport {
            responses: vec![
                Err(attempt_error(Error::new(ErrorKind::NotFound, "missing"))),
                Ok(ResponseParts {
                    status: 201,
                    metadata: etag_metadata("created-etag"),
                    diagnostics: Diagnostics::new(OperationClass::Metadata),
                }),
                Ok(ResponseParts {
                    status: 201,
                    metadata: etag_metadata("etag-existing"),
                    diagnostics: Diagnostics::new(OperationClass::Metadata),
                }),
                Err(attempt_error(Error::new(
                    ErrorKind::Unavailable,
                    "page create failed",
                ))),
            ],
            ..RecordingTransport::default()
        };
        let mut state = TenantInitState::default();
        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| TenantInitializer.initialize(cx, &mut transport, &plan, &mut state))
            .unwrap_err();

        assert_eq!(error.error.kind(), ErrorKind::Unavailable);
        assert!(!state.is_initialized());
        assert_eq!(transport.requests.len(), 4);
        assert!(
            !transport
                .requests
                .iter()
                .any(|request| request.metadata.get("x-ms-meta-initialized") == Some("true"))
        );
    }

    #[test]
    fn tenant_initializer_tolerates_existing_expected_pages_before_final_marker() {
        let plan = TenantInitPlan {
            containers: vec![ContainerTarget {
                account: AccountId::new("acct"),
                container: ContainerName::new("container"),
            }],
            metadata_object: object("metadata/root", ObjectKind::Metadata),
            metadata: MetadataMap::new(),
            page_objects: vec![PageObjectInit {
                object: object("data/page", ObjectKind::Data),
                content_len: 512,
            }],
        };
        let mut page_properties = MetadataMap::new();
        page_properties.insert("content-length", "512");
        let mut transport = RecordingTransport {
            responses: vec![
                Ok(ResponseParts {
                    status: 200,
                    metadata: etag_metadata("etag-existing"),
                    diagnostics: Diagnostics::new(OperationClass::Metadata),
                }),
                Ok(ResponseParts {
                    status: 201,
                    metadata: MetadataMap::new(),
                    diagnostics: Diagnostics::new(OperationClass::Metadata),
                }),
                Err(attempt_error(Error::new(
                    ErrorKind::AlreadyExists,
                    "page exists",
                ))),
                Ok(ResponseParts {
                    status: 200,
                    metadata: page_properties,
                    diagnostics: Diagnostics::new(OperationClass::Metadata),
                }),
                Ok(ResponseParts {
                    status: 200,
                    metadata: etag_metadata("etag-existing"),
                    diagnostics: Diagnostics::new(OperationClass::Metadata),
                }),
            ],
            ..RecordingTransport::default()
        };
        let mut state = TenantInitState::default();

        kimojio_stack::Runtime::new()
            .block_on(|cx| TenantInitializer.initialize(cx, &mut transport, &plan, &mut state))
            .unwrap();

        assert!(state.is_initialized());
        assert_eq!(
            transport.requests[2].metadata.get("if-none-match"),
            Some("*")
        );
        assert_eq!(transport.requests[3].method, "HEAD");
        assert_eq!(
            transport.requests[4].metadata.get("x-ms-meta-initialized"),
            Some("true")
        );
    }

    #[test]
    fn tenant_initializer_treats_condition_not_met_as_existing_page() {
        let plan = TenantInitPlan {
            containers: vec![ContainerTarget {
                account: AccountId::new("acct"),
                container: ContainerName::new("container"),
            }],
            metadata_object: object("metadata/root", ObjectKind::Metadata),
            metadata: MetadataMap::new(),
            page_objects: vec![PageObjectInit {
                object: object("data/page", ObjectKind::Data),
                content_len: 512,
            }],
        };
        let mut page_properties = MetadataMap::new();
        page_properties.insert("content-length", "512");
        let mut transport = RecordingTransport {
            responses: vec![
                Ok(ResponseParts {
                    status: 200,
                    metadata: etag_metadata("etag-existing"),
                    diagnostics: Diagnostics::new(OperationClass::Metadata),
                }),
                Ok(ResponseParts {
                    status: 201,
                    metadata: MetadataMap::new(),
                    diagnostics: Diagnostics::new(OperationClass::Metadata),
                }),
                Err(attempt_error(Error::new(
                    ErrorKind::ConditionNotMet,
                    "page exists",
                ))),
                Ok(ResponseParts {
                    status: 200,
                    metadata: page_properties,
                    diagnostics: Diagnostics::new(OperationClass::Metadata),
                }),
                Ok(ResponseParts {
                    status: 200,
                    metadata: MetadataMap::new(),
                    diagnostics: Diagnostics::new(OperationClass::Metadata),
                }),
            ],
            ..RecordingTransport::default()
        };
        let mut state = TenantInitState::default();

        kimojio_stack::Runtime::new()
            .block_on(|cx| TenantInitializer.initialize(cx, &mut transport, &plan, &mut state))
            .unwrap();

        assert!(state.is_initialized());
    }
}
