// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Container lifecycle request helpers.
//!
//! These helpers create, delete, and probe containers through the generic
//! [`Transport`] abstraction. Already-existing and missing-container responses
//! are converted into explicit outcomes so callers can write idempotent setup and
//! cleanup code without inspecting service error codes.

use crate::{
    AccountId, AttemptError, ContainerName, Diagnostics, Error, ErrorKind, OperationClass,
    RequestParts, StorageRuntime, Transport, model::percent_encode_component,
};

/// Result of creating a container.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainerCreateOutcome {
    /// Container was created.
    Created,
    /// Container already existed.
    AlreadyExists,
}

/// Result of deleting a container.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainerDeleteOutcome {
    /// Container was deleted.
    Deleted,
    /// Container was already absent.
    NotFound,
}

/// Client helper for container lifecycle operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContainerClient;

impl ContainerClient {
    /// Creates a container if it does not already exist.
    pub fn create<'cx, R, T>(
        self,
        cx: &'cx R::Context<'cx>,
        transport: &mut T,
        account: &AccountId,
        container: &ContainerName,
    ) -> Result<ContainerCreateOutcome, AttemptError>
    where
        R: StorageRuntime,
        T: Transport<R>,
    {
        match transport.execute(cx, &create_container_request(account, container)) {
            Ok(_) => Ok(ContainerCreateOutcome::Created),
            Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {
                Ok(ContainerCreateOutcome::AlreadyExists)
            }
            Err(error) => Err(error),
        }
    }

    /// Deletes a container if it exists.
    pub fn delete<'cx, R, T>(
        self,
        cx: &'cx R::Context<'cx>,
        transport: &mut T,
        account: &AccountId,
        container: &ContainerName,
    ) -> Result<ContainerDeleteOutcome, AttemptError>
    where
        R: StorageRuntime,
        T: Transport<R>,
    {
        match transport.execute(cx, &delete_container_request(account, container)) {
            Ok(_) => Ok(ContainerDeleteOutcome::Deleted),
            Err(error) if error.error.kind() == ErrorKind::NotFound => {
                Ok(ContainerDeleteOutcome::NotFound)
            }
            Err(error) => Err(error),
        }
    }

    /// Returns whether the container exists.
    pub fn exists<'cx, R, T>(
        self,
        cx: &'cx R::Context<'cx>,
        transport: &mut T,
        account: &AccountId,
        container: &ContainerName,
    ) -> Result<bool, AttemptError>
    where
        R: StorageRuntime,
        T: Transport<R>,
    {
        match transport.execute(cx, &container_exists_request(account, container)) {
            Ok(_) => Ok(true),
            Err(error) if error.error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

/// Builds an idempotent create-container request.
pub fn create_container_request(account: &AccountId, container: &ContainerName) -> RequestParts {
    let mut request = RequestParts::new(
        OperationClass::Metadata,
        "PUT",
        container_uri(account, container),
    );
    request.metadata.insert("content-length", "0");
    request.metadata.insert("if-none-match", "*");
    request
}

/// Builds a delete-container request.
pub fn delete_container_request(account: &AccountId, container: &ContainerName) -> RequestParts {
    let mut request = RequestParts::new(
        OperationClass::Metadata,
        "DELETE",
        container_uri(account, container),
    );
    request.metadata.insert("content-length", "0");
    request
}

/// Builds a container existence probe request.
pub fn container_exists_request(account: &AccountId, container: &ContainerName) -> RequestParts {
    RequestParts::new(
        OperationClass::Metadata,
        "HEAD",
        container_uri(account, container),
    )
}

fn container_uri(account: &AccountId, container: &ContainerName) -> String {
    format!(
        "/{}/{}?restype=container",
        percent_encode_component(account.as_str(), false),
        percent_encode_component(container.as_str(), false)
    )
}

/// Builds an attempt error with container-operation diagnostics.
pub fn container_attempt_error(kind: ErrorKind) -> AttemptError {
    AttemptError {
        error: Error::new(kind, "container operation failed"),
        diagnostics: Diagnostics::new(OperationClass::Metadata),
    }
}

#[cfg(test)]
mod tests {
    use crate::{MetadataMap, ResponseParts};

    use super::*;

    #[derive(Clone)]
    struct FakeTransport {
        request: Option<RequestParts>,
        response: Result<ResponseParts, AttemptError>,
    }

    impl Transport for FakeTransport {
        fn execute(
            &mut self,
            _cx: &kimojio_stack::RuntimeContext<'_>,
            request: &RequestParts,
        ) -> Result<ResponseParts, AttemptError> {
            self.request = Some(request.clone());
            self.response.clone()
        }
    }

    fn ok_transport() -> FakeTransport {
        FakeTransport {
            request: None,
            response: Ok(ResponseParts {
                status: 201,
                metadata: MetadataMap::new(),
                diagnostics: Diagnostics::new(OperationClass::Metadata),
            }),
        }
    }

    #[test]
    fn container_requests_include_idempotency_and_zero_length() {
        let account = AccountId::new("acct");
        let container = ContainerName::new("container");

        let create = create_container_request(&account, &container);
        assert_eq!(create.method, "PUT");
        assert_eq!(create.metadata.get("if-none-match"), Some("*"));
        assert_eq!(create.metadata.get("content-length"), Some("0"));

        let delete = delete_container_request(&account, &container);
        assert_eq!(delete.method, "DELETE");
        assert_eq!(delete.metadata.get("content-length"), Some("0"));
    }

    #[test]
    fn container_client_maps_idempotent_and_terminal_errors() {
        let account = AccountId::new("acct");
        let container = ContainerName::new("container");

        let outcome = kimojio_stack::Runtime::new()
            .block_on(|cx| ContainerClient.create(cx, &mut ok_transport(), &account, &container))
            .unwrap();
        assert_eq!(outcome, ContainerCreateOutcome::Created);

        let mut exists = ok_transport();
        exists.response = Err(container_attempt_error(ErrorKind::AlreadyExists));
        let outcome = kimojio_stack::Runtime::new()
            .block_on(|cx| ContainerClient.create(cx, &mut exists, &account, &container))
            .unwrap();
        assert_eq!(outcome, ContainerCreateOutcome::AlreadyExists);

        let mut being_deleted = ok_transport();
        being_deleted.response = Err(container_attempt_error(ErrorKind::BeingDeleted));
        assert_eq!(
            kimojio_stack::Runtime::new()
                .block_on(|cx| ContainerClient.create(cx, &mut being_deleted, &account, &container))
                .unwrap_err()
                .error
                .kind(),
            ErrorKind::BeingDeleted
        );

        let mut missing = ok_transport();
        missing.response = Err(container_attempt_error(ErrorKind::NotFound));
        let mut missing_delete = missing.clone();
        let deleted = kimojio_stack::Runtime::new()
            .block_on(|cx| ContainerClient.delete(cx, &mut missing_delete, &account, &container))
            .unwrap();
        assert_eq!(deleted, ContainerDeleteOutcome::NotFound);
        let exists = kimojio_stack::Runtime::new()
            .block_on(|cx| ContainerClient.exists(cx, &mut missing, &account, &container))
            .unwrap();
        assert!(!exists);

        let mut forbidden = ok_transport();
        forbidden.response = Err(container_attempt_error(ErrorKind::Authorization));
        assert_eq!(
            kimojio_stack::Runtime::new()
                .block_on(|cx| ContainerClient.exists(cx, &mut forbidden, &account, &container))
                .unwrap_err()
                .error
                .kind(),
            ErrorKind::Authorization
        );
    }
}
