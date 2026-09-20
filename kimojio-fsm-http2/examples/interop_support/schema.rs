use kimojio_fsm_http2::{Config, ConnectionResult, StreamOutcome};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, time::Duration};

pub const MAX_REQUESTS: usize = 4096;
pub const MAX_CONCURRENCY: usize = 64;
pub const MAX_BODY: u64 = 1024 * 1024 * 1024;
pub const MAX_INPUT: u64 = 2 * 1024 * 1024;
pub type Fields = Vec<(String, String)>;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Windows {
    pub stream_window: Option<u32>,
    pub connection_window: Option<u32>,
}

impl Windows {
    pub fn config(&self) -> Result<Config, String> {
        let defaults = Config::default();
        let stream = self.stream_window.unwrap_or(defaults.stream_receive_window);
        let connection = self
            .connection_window
            .unwrap_or(defaults.connection_receive_window);
        // A fixture bound, not a restriction on valid HTTP/2 window sizes.
        if stream > 1024 * 1024 || !(65_535..=4 * 1024 * 1024).contains(&connection) {
            return Err("receive windows exceed fixture bounds".into());
        }
        Ok(Config {
            http: kimojio_fsm_http2::HttpLimits::new()
                .set_max_body_bytes(MAX_BODY as usize)
                .set_max_active_streams(MAX_CONCURRENCY)
                .set_max_requests_per_connection(MAX_REQUESTS),
            stream_receive_window: stream,
            connection_receive_window: connection,
            // Smaller than a default DATA frame: exercise lease range resubmission.
            max_send_buffer_bytes: 8 * 1024,
            shutdown_timeout: Duration::from_millis(250),
            ..Config::default()
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
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
    pub requests: Vec<Request>,
    #[serde(default)]
    pub actions: Vec<Action>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerInput {
    pub schema: u32,
    #[serde(default)]
    pub config: Windows,
    pub timeout_ms: u64,
}

impl Input {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != 1
            || self.request_count == 0
            || self.request_count > MAX_REQUESTS
            || self.request_count != self.requests.len()
            || !(1..=MAX_CONCURRENCY).contains(&self.concurrency)
            || !(1..=120_000).contains(&self.timeout_ms)
            || self.port == 0
        {
            return Err("invalid schema, request count, concurrency, port, or timeout".into());
        }
        self.config.config()?;
        if self.requests.iter().any(|r| {
            r.body_bytes > MAX_BODY
                || r.method.len() > 32
                || r.path.len() > 8192
                || r.trailers.len() > 32
                || r.trailers.iter().any(|(n, v)| n.len() + v.len() > 8192)
        }) {
            return Err("request exceeds fixture limits".into());
        }
        let valid_stream = |id: u32| id % 2 == 1 && id / 2 < self.request_count as u32;
        let mut controlled = BTreeMap::new();
        for action in &self.actions {
            match *action {
                Action::Pause {
                    stream_id,
                    until_stream_ended,
                } => {
                    if !valid_stream(stream_id)
                        || !valid_stream(until_stream_ended)
                        || stream_id == until_stream_ended
                        || controlled.insert(stream_id, ()).is_some()
                    {
                        return Err("invalid or duplicate pause action".into());
                    }
                }
                Action::Reset {
                    stream_id, code, ..
                } => {
                    if !valid_stream(stream_id)
                        || code != 8
                        || controlled.insert(stream_id, ()).is_some()
                    {
                        return Err("reset supports one action per stream and code 8 only".into());
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
pub struct Error {
    pub scope: &'static str,
    pub code: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamTerminalOutcome {
    Complete,
    Reset,
    Unprocessed,
    ConnectionFailed,
    Deadline,
}

impl From<StreamOutcome> for StreamTerminalOutcome {
    fn from(outcome: StreamOutcome) -> Self {
        match outcome {
            StreamOutcome::Complete => Self::Complete,
            StreamOutcome::Reset(_) => Self::Reset,
            StreamOutcome::Unprocessed => Self::Unprocessed,
            StreamOutcome::ConnectionFailed => Self::ConnectionFailed,
            StreamOutcome::Deadline => Self::Deadline,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionTerminalOutcome {
    Graceful,
    PeerClosed,
    IoFailed,
    Protocol,
    ResourceExhausted,
}

impl From<ConnectionResult> for ConnectionTerminalOutcome {
    fn from(outcome: ConnectionResult) -> Self {
        match outcome {
            ConnectionResult::Graceful => Self::Graceful,
            ConnectionResult::PeerClosed => Self::PeerClosed,
            ConnectionResult::IoFailed => Self::IoFailed,
            ConnectionResult::Protocol(_) => Self::Protocol,
            ConnectionResult::ResourceExhausted => Self::ResourceExhausted,
        }
    }
}

/// A normal success requires `outcome == complete`, not only `ended` and a null error.
#[derive(Serialize)]
pub struct StreamReport {
    pub stream_id: u32,
    pub status: Option<u16>,
    pub content_length: Option<u64>,
    pub bytes: u64,
    pub sha256: String,
    pub trailers: Fields,
    pub informational: Vec<u16>,
    /// Records receive END_STREAM separately from upload and stream retirement.
    pub ended: bool,
    /// Null until retirement. A failed outcome invalidates a normal-success case.
    pub outcome: Option<StreamTerminalOutcome>,
    /// An actual HTTP/2 error code, not an invented code for a transport failure.
    pub error: Option<Error>,
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
            digest: Sha256::new(),
        }
    }
    pub fn finish(&mut self) {
        self.sha256 = format!("{:x}", self.digest.clone().finalize());
    }
}

#[derive(Serialize)]
pub struct ConnectionReport {
    pub error: Option<Error>,
    pub outcome: ConnectionTerminalOutcome,
    pub closed: bool,
}

#[derive(Serialize)]
pub struct Report {
    pub schema: u32,
    pub streams: Vec<StreamReport>,
    pub connection: ConnectionReport,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> Input {
        serde_json::from_str(
            r#"{"schema":1,"host":"127.0.0.1","port":1234,"timeout_ms":1000,
                "request_count":1,"concurrency":1,
                "requests":[{"method":"GET","path":"/"}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn limits_and_reserved_actions_are_explicit() {
        assert!(input().validate().is_ok());
        let mut i = input();
        i.concurrency = MAX_CONCURRENCY + 1;
        assert!(i.validate().is_err());
        i = input();
        i.request_count = 600;
        assert!(i.validate().is_err());
        i = input();
        i.requests[0].body_bytes = 16 * 1024 * 1024 + 17;
        assert!(i.validate().is_ok());
        assert!(serde_json::from_str::<Action>(r#"{"action":"unknown"}"#).is_err());
    }

    #[test]
    fn exact_schema_and_empty_digest() {
        let mut s = StreamReport::new(1);
        s.finish();
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["status"], serde_json::Value::Null);
        assert_eq!(v["error"], serde_json::Value::Null);
        assert_eq!(v["ended"], false);
        assert_eq!(v["outcome"], serde_json::Value::Null);
        assert_eq!(v["content_length"], serde_json::Value::Null);
        assert!(v.get("content_length").is_some());
        assert_eq!(
            v["sha256"],
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn terminal_failure_stays_visible_after_receive_end() {
        let mut report = StreamReport::new(1);
        report.ended = true;
        report.outcome = Some(StreamOutcome::ConnectionFailed.into());
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["ended"], true);
        assert_eq!(value["error"], serde_json::Value::Null);
        assert_eq!(value["outcome"], "connection_failed");
        assert_ne!(report.outcome, Some(StreamTerminalOutcome::Complete));
    }

    #[test]
    fn terminal_outcomes_have_explicit_stable_names() {
        for (outcome, expected) in [
            (StreamOutcome::Complete, "complete"),
            (StreamOutcome::Reset(8), "reset"),
            (StreamOutcome::Unprocessed, "unprocessed"),
            (StreamOutcome::ConnectionFailed, "connection_failed"),
            (StreamOutcome::Deadline, "deadline"),
        ] {
            assert_eq!(
                serde_json::to_value(StreamTerminalOutcome::from(outcome)).unwrap(),
                expected
            );
        }
        for (outcome, expected) in [
            (ConnectionResult::Graceful, "graceful"),
            (ConnectionResult::PeerClosed, "peer_closed"),
            (ConnectionResult::IoFailed, "io_failed"),
            (ConnectionResult::ResourceExhausted, "resource_exhausted"),
        ] {
            assert_eq!(
                serde_json::to_value(ConnectionTerminalOutcome::from(outcome)).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn server_json_and_absent_windows_preserve_native_defaults() {
        let server: ServerInput =
            serde_json::from_str(r#"{"schema":1,"config":{},"timeout_ms":60000}"#).unwrap();
        let config = server.config.config().unwrap();
        assert_eq!(server.schema, 1);
        assert_eq!(server.timeout_ms, 60000);
        assert_eq!(
            config.stream_receive_window,
            Config::default().stream_receive_window
        );
        assert_eq!(
            config.connection_receive_window,
            Config::default().connection_receive_window
        );
    }
}
