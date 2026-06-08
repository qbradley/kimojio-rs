// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::VecDeque;

use bytes::Bytes;

use crate::{
    AttemptDiagnostics, AttemptError, Diagnostics, Error, ErrorKind, MetadataMap, OperationClass,
    RequestParts, ResponseParts, RetryObservation, Transport,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FakeResponse {
    pub status: u16,
    pub metadata: MetadataMap,
    pub body: Vec<Bytes>,
}

impl FakeResponse {
    pub fn ok(body: impl Into<Bytes>) -> Self {
        Self {
            status: 200,
            metadata: MetadataMap::new(),
            body: vec![body.into()],
        }
    }
}

#[derive(Debug, Default)]
pub struct FakeService {
    responses: VecDeque<Result<FakeResponse, ErrorKind>>,
    requests: Vec<RequestParts>,
    retry_observations: Vec<RetryObservation>,
}

impl FakeService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_response(&mut self, response: FakeResponse) {
        self.responses.push_back(Ok(response));
    }

    pub fn push_error(&mut self, kind: ErrorKind) {
        self.responses.push_back(Err(kind));
    }

    pub fn record_retry(&mut self, observation: RetryObservation) {
        self.retry_observations.push(observation);
    }

    pub fn requests(&self) -> &[RequestParts] {
        &self.requests
    }

    pub fn retry_observations(&self) -> &[RetryObservation] {
        &self.retry_observations
    }
}

impl Transport for FakeService {
    fn execute(
        &mut self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        request: &RequestParts,
    ) -> Result<ResponseParts, AttemptError> {
        let mut discard = |_| Ok(());
        self.execute_with_body_chunks(cx, request, &mut discard)
    }

    fn execute_with_body_chunks(
        &mut self,
        _cx: &kimojio_stack::RuntimeContext<'_>,
        request: &RequestParts,
        on_chunk: &mut dyn FnMut(Bytes) -> Result<(), Error>,
    ) -> Result<ResponseParts, AttemptError> {
        self.requests.push(request.clone());
        let response = match self
            .responses
            .pop_front()
            .unwrap_or(Err(ErrorKind::Transport))
        {
            Ok(response) => response,
            Err(kind) => return Err(fake_attempt_error(kind, request.operation)),
        };
        for chunk in response.body {
            on_chunk(chunk).map_err(|error| AttemptError {
                error,
                diagnostics: Diagnostics::new(request.operation),
            })?;
        }
        Ok(ResponseParts {
            status: response.status,
            metadata: response.metadata,
            diagnostics: Diagnostics::new(request.operation),
        })
    }
}

fn fake_attempt_error(kind: ErrorKind, operation: OperationClass) -> AttemptError {
    let mut diagnostics = Diagnostics::new(operation);
    diagnostics.push_attempt(AttemptDiagnostics {
        attempt: 1,
        status: None,
        service_code: None,
        request_id: Some("fake-request".into()),
        elapsed: None,
        retriable: matches!(
            kind,
            ErrorKind::Timeout | ErrorKind::Transport | ErrorKind::Unavailable
        ),
    });
    AttemptError {
        error: Error::new(kind, "fake service error"),
        diagnostics,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeterministicEmulator {
    pub account: String,
    pub secure: bool,
}

impl DeterministicEmulator {
    pub fn endpoint(&self) -> String {
        let scheme = if self.secure { "https" } else { "http" };
        format!("{scheme}://127.0.0.1/{}", self.account)
    }
}

#[cfg(test)]
mod tests {
    use crate::{RetryDecision, RetryObservation};

    use super::*;

    #[test]
    fn fake_service_records_requests_faults_and_retry_observations() {
        let mut service = FakeService::new();
        service.push_error(ErrorKind::Unavailable);
        service.record_retry(RetryObservation {
            attempt: 1,
            decision: RetryDecision::RetryAfter(std::time::Duration::ZERO),
            error_kind: ErrorKind::Unavailable,
        });
        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| {
                service.execute(cx, &RequestParts::new(OperationClass::List, "GET", "/list"))
            })
            .unwrap_err();

        assert_eq!(error.error.kind(), ErrorKind::Unavailable);
        assert_eq!(error.diagnostics.operation(), OperationClass::List);
        assert_eq!(service.requests().len(), 1);
        assert_eq!(service.retry_observations().len(), 1);
    }

    #[test]
    fn deterministic_emulator_endpoint_is_stable() {
        let emulator = DeterministicEmulator {
            account: "devstore".into(),
            secure: false,
        };

        assert_eq!(emulator.endpoint(), "http://127.0.0.1/devstore");
    }

    #[test]
    fn fake_service_fails_when_response_queue_is_exhausted() {
        let mut service = FakeService::new();
        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| {
                service.execute(
                    cx,
                    &RequestParts::new(OperationClass::PageRead, "GET", "/page"),
                )
            })
            .unwrap_err();

        assert_eq!(error.error.kind(), ErrorKind::Transport);
        assert_eq!(error.diagnostics.operation(), OperationClass::PageRead);
    }
}
