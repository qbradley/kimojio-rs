use kimojio_http2::{Config, ConnectionResult, Error, StreamOutcome};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, net::IpAddr, time::Duration};

pub const MAX_INPUT: u64 = 2 * 1024 * 1024;
pub const MAX_REQUESTS: usize = 4096;
pub const MAX_CONCURRENCY: usize = 64;
pub const MAX_BODY: u64 = 1024 * 1024 * 1024;
pub const MAX_METADATA: usize = 8 * 1024 * 1024;
pub type Fields = Vec<(String, String)>;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Windows {
    pub stream_window: Option<u32>,
    pub connection_window: Option<u32>,
}

impl Windows {
    pub fn config(&self) -> Result<Config, String> {
        let mut config = Config::default();
        if let Some(window) = self.stream_window {
            if window > 1024 * 1024 {
                return Err("stream window exceeds fixture bound".into());
            }
            config.protocol.stream_receive_window = window;
        }
        if let Some(window) = self.connection_window {
            if !(65_535..=4 * 1024 * 1024).contains(&window) {
                return Err("connection window exceeds fixture bounds".into());
            }
            config.protocol.connection_receive_window = window;
        }
        config.protocol.http = config
            .protocol
            .http
            .set_max_body_bytes(MAX_BODY as usize)
            .set_max_requests_per_connection(MAX_REQUESTS);
        config.protocol.shutdown_timeout = Duration::from_millis(250);
        Ok(config)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestSpec {
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub body_bytes: u64,
    #[serde(default)]
    pub trailers: Fields,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Pause {
        stream_id: u32,
        until_stream_ended: u32,
    },
    Reset {
        stream_id: u32,
        after_bytes: u64,
        code: u32,
    },
    GracefulClose {
        after_streams: usize,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub schema: u32,
    pub host: String,
    pub port: u16,
    pub timeout_ms: u64,
    #[serde(default)]
    pub config: Windows,
    pub request_count: usize,
    pub concurrency: usize,
    pub requests: Vec<RequestSpec>,
    #[serde(default)]
    pub actions: Vec<Action>,
}

impl Input {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != 1
            || !(1..=MAX_REQUESTS).contains(&self.request_count)
            || self.request_count != self.requests.len()
            || !(1..=MAX_CONCURRENCY).contains(&self.concurrency)
            || !(1..=120_000).contains(&self.timeout_ms)
            || self.port == 0
        {
            return Err("invalid schema, request count, concurrency, port, or timeout".into());
        }
        if !self
            .host
            .parse::<IpAddr>()
            .map_err(|e| e.to_string())?
            .is_loopback()
        {
            return Err("fixture requires a loopback IP literal".into());
        }
        self.config.config()?;
        for request in &self.requests {
            if request.body_bytes > MAX_BODY
                || request.method.len() > 32
                || request.path.len() > 8192
                || request.trailers.len() > 32
                || request
                    .trailers
                    .iter()
                    .any(|(n, v)| n.len() + v.len() > 8192)
            {
                return Err("request exceeds fixture bounds".into());
            }
            if request.path.starts_with("/informational/") {
                return Err(
                    "unsupported: this wrapper has no informational-response callback".into(),
                );
            }
        }
        let valid = |id: u32| id % 2 == 1 && id / 2 < self.request_count as u32;
        let mut controlled = BTreeSet::new();
        for action in &self.actions {
            match *action {
                Action::Pause {
                    stream_id,
                    until_stream_ended,
                } => {
                    if !valid(stream_id)
                        || !valid(until_stream_ended)
                        || stream_id == until_stream_ended
                        || !controlled.insert(stream_id)
                    {
                        return Err("invalid or duplicate pause action".into());
                    }
                }
                Action::Reset {
                    stream_id, code, ..
                } => {
                    if !valid(stream_id) || code != 8 || !controlled.insert(stream_id) {
                        return Err("reset requires a unique stream and CANCEL code 8".into());
                    }
                }
                Action::GracefulClose { after_streams } => {
                    if after_streams == 0 || after_streams > self.request_count {
                        return Err("invalid graceful_close threshold".into());
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct WireError {
    pub scope: &'static str,
    pub code: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Complete,
    Reset,
    Unprocessed,
    ConnectionFailed,
    Deadline,
}

impl From<StreamOutcome> for Outcome {
    fn from(value: StreamOutcome) -> Self {
        match value {
            StreamOutcome::Complete => Self::Complete,
            StreamOutcome::Reset(_) => Self::Reset,
            StreamOutcome::Unprocessed => Self::Unprocessed,
            StreamOutcome::ConnectionFailed => Self::ConnectionFailed,
            StreamOutcome::Deadline => Self::Deadline,
        }
    }
}

#[derive(Serialize)]
pub struct StreamReport {
    pub stream_id: u32,
    pub status: Option<u16>,
    pub content_length: Option<u64>,
    pub bytes: u64,
    pub sha256: String,
    pub trailers: Fields,
    pub informational: Vec<u16>,
    pub ended: bool,
    /// Only an authoritative full-retirement outcome can establish success.
    pub outcome: Option<Outcome>,
    pub error: Option<WireError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrapper_error: Option<String>,
    #[serde(skip)]
    pub digest: Sha256,
}

impl StreamReport {
    pub fn new(stream_id: u32) -> Self {
        Self {
            stream_id,
            status: None,
            content_length: None,
            bytes: 0,
            sha256: String::new(),
            trailers: Vec::new(),
            informational: Vec::new(),
            ended: false,
            outcome: None,
            error: None,
            wrapper_error: None,
            digest: Sha256::new(),
        }
    }
    pub fn receive(&mut self, outcome: Option<StreamOutcome>) {
        self.ended |= outcome == Some(StreamOutcome::Complete);
        if let Some(StreamOutcome::Reset(code)) = outcome {
            self.error = Some(WireError {
                scope: "stream",
                code,
            });
        }
    }
    pub fn completion(&mut self, result: Result<StreamOutcome, Error>) -> Result<(), String> {
        let outcome = match result {
            Ok(outcome) | Err(Error::Stream(outcome)) => outcome,
            Err(error) => {
                self.note_error(&error);
                return Err(format!(
                    "unsupported: stream {} completion returned {error:?}, not an observable retirement outcome",
                    self.stream_id
                ));
            }
        };
        self.outcome = Some(outcome.into());
        if let StreamOutcome::Reset(code) = outcome {
            self.error = Some(WireError {
                scope: "stream",
                code,
            });
        }
        Ok(())
    }
    pub fn note_error(&mut self, error: &Error) {
        self.wrapper_error = Some(format!("{error:?}"));
        if let Error::Stream(StreamOutcome::Reset(code))
        | Error::Send {
            reason: kimojio_http2::SendStop::Reset(code),
            ..
        } = error
        {
            self.error = Some(WireError {
                scope: "stream",
                code: *code,
            });
        }
    }
    pub fn finish(&mut self) {
        self.sha256 = format!("{:x}", self.digest.clone().finalize());
    }
}

#[derive(Serialize)]
pub struct ConnectionReport {
    pub outcome: Option<&'static str>,
    pub error: Option<WireError>,
    pub closed: bool,
}

impl ConnectionReport {
    pub fn pending() -> Self {
        Self {
            outcome: None,
            error: None,
            closed: false,
        }
    }
    pub fn driver(result: Result<(), Error>) -> Result<Self, String> {
        let outcome = match result {
            Ok(()) => ConnectionResult::Graceful,
            Err(Error::Connection(outcome)) => outcome,
            Err(error) => {
                return Err(format!(
                    "native driver did not expose confirmed close and connection outcome: {error:?}"
                ));
            }
        };
        let name = match outcome {
            ConnectionResult::Graceful => "graceful",
            ConnectionResult::PeerClosed => "peer_closed",
            ConnectionResult::IoFailed => "io_failed",
            ConnectionResult::Protocol(_) => "protocol",
            ConnectionResult::ResourceExhausted => "resource_exhausted",
            ConnectionResult::Aborted => "aborted",
        };
        let error = if let ConnectionResult::Protocol(error) = outcome {
            Some(WireError {
                scope: "connection",
                code: error.code.as_u32(),
            })
        } else {
            None
        };
        Ok(Self {
            outcome: Some(name),
            error,
            closed: true,
        })
    }
}

#[derive(Serialize)]
pub struct Report {
    pub schema: u32,
    pub streams: Vec<StreamReport>,
    pub connection: ConnectionReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixture_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_limits_and_unobservable_information_are_explicit_errors() {
        let parse = || {
            serde_json::from_str::<Input>(
                r#"{"schema":1,"host":"127.0.0.1","port":1234,"timeout_ms":1000,
            "request_count":1,"concurrency":1,"requests":[{"method":"GET","path":"/"}]}"#,
            )
            .unwrap()
        };
        assert!(parse().validate().is_ok());
        let mut input = parse();
        input.concurrency = MAX_CONCURRENCY + 1;
        assert!(input.validate().is_err());
        input = parse();
        input.requests[0].path = "/informational/37".into();
        assert!(input.validate().unwrap_err().contains("unsupported"));
        assert!(
            serde_json::from_str::<Action>(
                r#"{"action":"cancel_upload_after_response","stream_id":1}"#
            )
            .is_err()
        );
    }

    #[test]
    fn omitted_windows_preserve_wrapper_defaults() {
        let config = Windows::default().config().unwrap();
        let defaults = Config::default();
        assert_eq!(
            config.protocol.stream_receive_window,
            defaults.protocol.stream_receive_window
        );
        assert_eq!(
            config.protocol.connection_receive_window,
            defaults.protocol.connection_receive_window
        );
    }

    #[test]
    fn receive_success_cannot_mask_unknown_retirement() {
        let mut report = StreamReport::new(1);
        report.receive(Some(StreamOutcome::Complete));
        let result = report.completion(Err(Error::Send {
            accepted: 1024,
            exact: true,
            reason: kimojio_http2::SendStop::Reset(0),
        }));
        assert!(result.is_err());
        assert!(report.ended);
        assert!(report.outcome.is_none());
        assert_eq!(report.error.unwrap().code, 0);
        assert!(report.wrapper_error.unwrap().contains("accepted: 1024"));
    }

    #[test]
    fn zero_wire_reset_preserves_completed_response() {
        let mut report = StreamReport::new(1);
        report.receive(Some(StreamOutcome::Complete));
        report
            .completion(Err(Error::Stream(StreamOutcome::Reset(0))))
            .unwrap();
        assert!(report.ended);
        assert_eq!(report.outcome, Some(Outcome::Reset));
        assert_eq!(report.error.unwrap().code, 0);
    }

    #[test]
    fn only_driver_completion_confirms_actual_close() {
        assert!(!ConnectionReport::pending().closed);
        let failed =
            ConnectionReport::driver(Err(Error::Connection(ConnectionResult::IoFailed))).unwrap();
        assert!(failed.closed);
        assert!(failed.error.is_none());
        assert_eq!(failed.outcome, Some("io_failed"));
        assert!(ConnectionReport::driver(Err(Error::Closed)).is_err());
    }

    #[test]
    fn nullable_fields_are_explicit() {
        let mut report = StreamReport::new(1);
        report.finish();
        let value = serde_json::to_value(report).unwrap();
        assert!(value["outcome"].is_null());
        assert!(value.get("content_length").unwrap().is_null());
        assert_eq!(
            value["sha256"],
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
