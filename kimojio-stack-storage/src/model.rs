// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core storage identifiers, metadata, and caller configuration.
//!
//! The model types are intentionally small wrappers around strings and maps. They
//! keep object identity explicit while leaving validation policy to callers or
//! service-specific helpers. Metadata keys are normalized to lower case so
//! service headers and user metadata can be queried consistently.
//!
//! ```
//! use kimojio_stack_storage::{
//!     AccountId, ContainerName, MetadataMap, ObjectKind, ObjectName, ObjectRef,
//! };
//!
//! let object = ObjectRef {
//!     account: AccountId::new("acct"),
//!     container: ContainerName::new("container"),
//!     name: ObjectName::new("dir/object"),
//!     kind: ObjectKind::Data,
//! };
//! assert_eq!(object.path(), "acct/container/dir/object");
//!
//! let mut metadata = MetadataMap::new();
//! metadata.insert("Upload-LSN", "42");
//! assert_eq!(metadata.get("upload-lsn"), Some("42"));
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use crate::AuthMode;

/// Storage account identifier.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AccountId(String);

impl AccountId {
    /// Creates an account identifier from caller-owned text.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the account identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Container name.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ContainerName(String);

impl ContainerName {
    /// Creates a container name from caller-owned text.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the container name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Object name.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ObjectName(String);

impl ObjectName {
    /// Creates an object name from caller-owned text.
    ///
    /// Slash separators are preserved in object paths.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the object name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Object category used for routing and operation behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectKind {
    Internal,
    Metadata,
    Data,
    Backup,
    Status,
    Config,
    Test,
}

/// Fully routed object reference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectRef {
    pub account: AccountId,
    pub container: ContainerName,
    pub name: ObjectName,
    pub kind: ObjectKind,
}

impl ObjectRef {
    /// Returns a human-readable `account/container/name` path.
    ///
    /// This is for diagnostics and local routing. Request builders percent-encode
    /// the path internally before sending it on the wire.
    pub fn path(&self) -> String {
        format!(
            "{}/{}/{}",
            self.account.as_str(),
            self.container.as_str(),
            self.name.as_str()
        )
    }

    pub(crate) fn encoded_path(&self) -> String {
        format!(
            "{}/{}/{}",
            percent_encode_component(self.account.as_str(), false),
            percent_encode_component(self.container.as_str(), false),
            percent_encode_component(self.name.as_str(), true)
        )
    }
}

pub(crate) fn percent_encode_component(value: &str, preserve_slash: bool) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            b'/' if preserve_slash => encoded.push('/'),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Metadata map preserving caller-provided metadata keys and values.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct MetadataMap {
    entries: BTreeMap<String, String>,
}

impl fmt::Debug for MetadataMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = self
            .entries
            .iter()
            .map(|(key, value)| {
                (
                    key.as_str(),
                    if should_redact_metadata(key, value) {
                        "<redacted>"
                    } else {
                        value.as_str()
                    },
                )
            })
            .collect::<Vec<_>>();
        f.debug_map().entries(entries).finish()
    }
}

impl MetadataMap {
    /// Creates an empty metadata map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a key/value pair using the normalized lower-case key.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) -> Option<String> {
        self.entries
            .insert(normalize_metadata_key(key), value.into())
    }

    /// Returns a value using normalized key lookup.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .get(&normalize_metadata_key(key))
            .map(String::as_str)
    }

    /// Returns the normalized entries.
    pub fn entries(&self) -> &BTreeMap<String, String> {
        &self.entries
    }
}

fn normalize_metadata_key(key: impl Into<String>) -> String {
    key.into().to_ascii_lowercase()
}

fn should_redact_metadata(key: &str, value: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("authorization")
        || key.contains("token")
        || key.contains("secret")
        || key.contains("sig")
        || key.contains("key")
        || value.contains("sig=")
        || value.contains("sig%3d")
}

/// Ownership lease and epoch context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeaseContext {
    pub lease_id: String,
    pub epoch: u64,
}

/// Object properties used by page/block operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectProperties {
    pub content_len: u64,
    pub etag: Option<String>,
    pub sequence_number: Option<u64>,
    pub snapshot: Option<String>,
    pub metadata: MetadataMap,
}

/// Connection reuse and timeout settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PoolConfig {
    pub keepalive: bool,
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
    pub total_timeout: Duration,
    pub max_idle_per_host: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            keepalive: true,
            idle_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            total_timeout: Duration::from_secs(60),
            max_idle_per_host: 8,
        }
    }
}

/// Caller and client concurrency limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConcurrencyConfig {
    pub max_in_flight: usize,
    pub max_per_operation: usize,
}

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self {
            max_in_flight: 128,
            max_per_operation: 32,
        }
    }
}

/// Storage context configured by callers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageContext {
    pub endpoint: String,
    pub container: ContainerName,
    pub auth: AuthMode,
    pub emulator: bool,
    pub pool: PoolConfig,
    pub concurrency: ConcurrencyConfig,
}

impl StorageContext {
    /// Creates storage context with default pool and concurrency limits.
    pub fn new(endpoint: impl Into<String>, container: ContainerName, auth: AuthMode) -> Self {
        Self {
            endpoint: endpoint.into(),
            container,
            auth,
            emulator: false,
            pool: PoolConfig::default(),
            concurrency: ConcurrencyConfig::default(),
        }
    }

    /// Marks whether requests should target an emulator-compatible endpoint.
    pub fn with_emulator(mut self, emulator: bool) -> Self {
        self.emulator = emulator;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_reference_formats_account_container_and_name() {
        let object = ObjectRef {
            account: AccountId::new("account"),
            container: ContainerName::new("container"),
            name: ObjectName::new("path/blob"),
            kind: ObjectKind::Data,
        };

        assert_eq!(object.path(), "account/container/path/blob");
        assert_eq!(object.encoded_path(), "account/container/path/blob");
        let object = ObjectRef {
            account: AccountId::new("acct space"),
            container: ContainerName::new("container"),
            name: ObjectName::new("a b&c?d#e"),
            kind: ObjectKind::Data,
        };
        assert_eq!(
            object.encoded_path(),
            "acct%20space/container/a%20b%26c%3Fd%23e"
        );
    }

    #[test]
    fn storage_context_preserves_auth_and_limits() {
        let context = StorageContext::new(
            "https://example.invalid",
            ContainerName::new("container"),
            AuthMode::Bearer {
                token: "token".into(),
            },
        )
        .with_emulator(true);

        assert!(context.emulator);
        assert_eq!(context.pool.max_idle_per_host, 8);
        assert_eq!(context.concurrency.max_in_flight, 128);
        assert_eq!(context.auth.mode_name(), "bearer");
    }

    #[test]
    fn metadata_map_normalizes_keys() {
        let mut metadata = MetadataMap::new();
        metadata.insert("Highest_LSN", "42");

        assert_eq!(metadata.get("highest_lsn"), Some("42"));
        assert_eq!(metadata.get("HIGHEST_LSN"), Some("42"));
        assert_eq!(metadata.entries().keys().next().unwrap(), "highest_lsn");
    }

    #[test]
    fn metadata_debug_redacts_sensitive_values() {
        let mut metadata = MetadataMap::new();
        metadata.insert("x-ms-copy-source-authorization", "Bearer secret-token");
        metadata.insert(
            "x-ms-copy-source",
            "https://example.invalid/object?sig=secret",
        );
        metadata.insert("content-length", "10");
        let debug = format!("{metadata:?}");

        assert!(!debug.contains("secret-token"));
        assert!(!debug.contains("sig=secret"));
        assert!(debug.contains("content-length"));
    }
}
