use kimojio_fsm_http2::Config;
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
        let stream = self.stream_window.unwrap_or(65_535);
        let connection = self.connection_window.unwrap_or(65_535);
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

#[derive(Serialize)]
pub struct StreamReport {
    pub stream_id: u32,
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_length: Option<u64>,
    pub bytes: u64,
    pub sha256: String,
    pub trailers: Fields,
    pub informational: Vec<u16>,
    pub ended: bool,
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
            error: None,
            digest: Sha256::new(),
        }
    }
    pub fn finish(&mut self) {
        self.sha256 = format!("{:x}", self.digest.clone().finalize());
    }
}

#[derive(Default, Serialize)]
pub struct ConnectionReport {
    pub error: Option<Error>,
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
        assert!(v.get("content_length").is_none());
        assert_eq!(
            v["sha256"],
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(!ConnectionReport::default().closed);
    }
}
