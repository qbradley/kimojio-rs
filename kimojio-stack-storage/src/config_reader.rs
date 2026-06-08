// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ETag-aware JSON configuration reads.
//!
//! [`ConfigReader`] reads a JSON object from storage and updates caller-owned
//! [`ConfigState`] with the latest ETag. Subsequent reads send `if-none-match`
//! and can return [`ConfigReadOutcome::Unchanged`] without requiring callers to
//! compare response metadata manually.
//!
//! ```
//! use kimojio_stack_storage::{ConfigState, parse_config_json};
//!
//! let value = parse_config_json(br#"{"version":1}"#).unwrap();
//! assert_eq!(value["version"], 1);
//! let state = ConfigState::default();
//! assert!(state.etag.is_none());
//! ```

use bytes::BytesMut;
use serde_json::Value;

use crate::{
    AttemptError, Conditions, Diagnostics, Error, ErrorKind, ObjectRef, OperationClass,
    RequestParts, Transport,
};

/// Outcome of one config read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigReadOutcome {
    /// New JSON value was fetched and the ETag changed or was unknown.
    Updated { value: Value, etag: String },
    /// Caller already has the current value for this ETag.
    Unchanged { etag: String },
    /// Config object was not found.
    Missing,
}

/// Caller-owned config cache state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConfigState {
    /// Last observed ETag, if any.
    pub etag: Option<String>,
}

/// Client helper for ETag-aware config reads.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConfigReader;

impl ConfigReader {
    /// Reads the config object, using `state.etag` as an `if-none-match` condition.
    pub fn read<T: Transport>(
        self,
        cx: &kimojio_stack::RuntimeContext<'_>,
        transport: &mut T,
        object: &ObjectRef,
        state: &mut ConfigState,
    ) -> Result<ConfigReadOutcome, AttemptError> {
        let request = config_request(object, state.etag.as_deref());
        let mut body = BytesMut::new();
        let response = match transport.execute_with_body_chunks(cx, &request, &mut |chunk| {
            body.extend_from_slice(&chunk);
            Ok(())
        }) {
            Ok(response) => response,
            Err(error) if error.error.kind() == ErrorKind::NotFound => {
                return Ok(ConfigReadOutcome::Missing);
            }
            Err(error) if error.error.kind() == ErrorKind::ConditionNotMet => {
                let etag = state.etag.clone().unwrap_or_default();
                return Ok(ConfigReadOutcome::Unchanged { etag });
            }
            Err(error) => return Err(error),
        };
        if response.status == 304 {
            let etag = state.etag.clone().unwrap_or_default();
            return Ok(ConfigReadOutcome::Unchanged { etag });
        }
        let value = parse_config_json(&body).map_err(attempt_error)?;
        let etag = response
            .metadata
            .get("etag")
            .filter(|etag| !etag.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                attempt_error(Error::new(
                    ErrorKind::Corruption,
                    "config response is missing a non-empty ETag",
                ))
            })?;
        state.etag = Some(etag.clone());
        Ok(ConfigReadOutcome::Updated { value, etag })
    }
}

/// Builds a config read request with an optional known ETag.
pub fn config_request(object: &ObjectRef, known_etag: Option<&str>) -> RequestParts {
    let mut request = RequestParts::new(
        OperationClass::Config,
        "GET",
        format!("/{}", object.encoded_path()),
    );
    if let Some(etag) = known_etag {
        Conditions::if_none_match(etag).apply(&mut request);
    }
    request
}

/// Parses config bytes and requires a JSON object root.
pub fn parse_config_json(bytes: &[u8]) -> Result<Value, Error> {
    let value = serde_json::from_slice::<Value>(bytes)
        .map_err(|error| Error::new(ErrorKind::Corruption, error.to_string()))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(Error::new(
            ErrorKind::Corruption,
            "config JSON root must be an object",
        ))
    }
}

fn attempt_error(error: Error) -> AttemptError {
    AttemptError {
        error,
        diagnostics: Diagnostics::new(OperationClass::Config),
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use crate::{AccountId, ContainerName, MetadataMap, ObjectKind, ObjectName, ResponseParts};

    use super::*;

    struct FakeTransport {
        chunks: Vec<Bytes>,
        response: Result<ResponseParts, AttemptError>,
        requests: Vec<RequestParts>,
    }

    impl Default for FakeTransport {
        fn default() -> Self {
            Self {
                chunks: Vec::new(),
                response: Ok(ResponseParts {
                    status: 200,
                    metadata: MetadataMap::new(),
                    diagnostics: Diagnostics::new(OperationClass::Config),
                }),
                requests: Vec::new(),
            }
        }
    }

    impl Transport for FakeTransport {
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
            for chunk in &self.chunks {
                on_chunk(chunk.clone()).map_err(attempt_error)?;
            }
            self.response.clone()
        }
    }

    fn object() -> ObjectRef {
        ObjectRef {
            account: AccountId::new("acct"),
            container: ContainerName::new("container"),
            name: ObjectName::new("config.json"),
            kind: ObjectKind::Config,
        }
    }

    #[test]
    fn config_reader_reads_json_tracks_etag_and_handles_unchanged_missing() {
        let mut metadata = MetadataMap::new();
        metadata.insert("etag", "etag-1");
        let mut transport = FakeTransport {
            chunks: vec![Bytes::from_static(br#"{"version":1}"#)],
            response: Ok(ResponseParts {
                status: 200,
                metadata,
                diagnostics: Diagnostics::new(OperationClass::Config),
            }),
            requests: Vec::new(),
        };
        let mut state = ConfigState::default();
        let outcome = kimojio_stack::Runtime::new()
            .block_on(|cx| ConfigReader.read(cx, &mut transport, &object(), &mut state))
            .unwrap();
        assert!(matches!(outcome, ConfigReadOutcome::Updated { .. }));
        assert_eq!(state.etag.as_deref(), Some("etag-1"));

        let request = config_request(&object(), state.etag.as_deref());
        assert_eq!(request.metadata.get("if-none-match"), Some("etag-1"));

        let mut unchanged = FakeTransport {
            response: Ok(ResponseParts {
                status: 304,
                metadata: MetadataMap::new(),
                diagnostics: Diagnostics::new(OperationClass::Config),
            }),
            ..FakeTransport::default()
        };
        assert_eq!(
            kimojio_stack::Runtime::new()
                .block_on(|cx| ConfigReader.read(cx, &mut unchanged, &object(), &mut state))
                .unwrap(),
            ConfigReadOutcome::Unchanged {
                etag: "etag-1".into()
            }
        );

        let mut missing = FakeTransport {
            response: Err(attempt_error(Error::new(ErrorKind::NotFound, "missing"))),
            ..FakeTransport::default()
        };
        assert_eq!(
            kimojio_stack::Runtime::new()
                .block_on(|cx| ConfigReader.read(cx, &mut missing, &object(), &mut state))
                .unwrap(),
            ConfigReadOutcome::Missing
        );
    }

    #[test]
    fn config_json_root_must_be_object() {
        assert!(parse_config_json(br#"{"ok":true}"#).is_ok());
        assert_eq!(
            parse_config_json(b"[]").unwrap_err().kind(),
            ErrorKind::Corruption
        );
    }

    #[test]
    fn config_reader_rejects_updated_config_without_etag() {
        let mut transport = FakeTransport {
            chunks: vec![Bytes::from_static(br#"{"version":1}"#)],
            ..FakeTransport::default()
        };
        let mut state = ConfigState::default();
        let error = kimojio_stack::Runtime::new()
            .block_on(|cx| ConfigReader.read(cx, &mut transport, &object(), &mut state))
            .unwrap_err();

        assert_eq!(error.error.kind(), ErrorKind::Corruption);
        assert_eq!(state.etag, None);
    }
}
