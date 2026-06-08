// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Typed metadata helpers.
//!
//! Storage services expose metadata as strings. [`TypedMetadata`] parses and
//! writes the metadata keys used by this crate while preserving the lower-level
//! [`MetadataMap`] representation for request builders.
//!
//! ```
//! use kimojio_stack_storage::{TypedMetadata, UPLOAD_LSN};
//!
//! let typed = TypedMetadata { upload_lsn: Some(42), ..TypedMetadata::default() };
//! let metadata = typed.to_metadata_map();
//! assert_eq!(metadata.get(UPLOAD_LSN), Some("42"));
//! ```

use crate::{Error, ErrorKind, MetadataMap};

/// Highest durable log sequence number metadata key.
pub const HIGH_WATER_LSN: &str = "high-water-lsn";
/// Oldest replay log sequence number metadata key.
pub const OLDEST_REPLAY_LSN: &str = "oldest-replay-lsn";
/// Relation size metadata key.
pub const RELATION_SIZE: &str = "relation-size";
/// Deletion marker metadata key.
pub const DELETION_MARKER: &str = "deletion-marker";
/// Truncation log sequence number metadata key.
pub const TRUNCATION_LSN: &str = "truncation-lsn";
/// Shard config metadata key.
pub const SHARD_CONFIG: &str = "shard-config";
/// Upload log sequence number metadata key.
pub const UPLOAD_LSN: &str = "upload-lsn";
/// Backup time metadata key.
pub const BACKUP_TIME: &str = "backup-time";
/// Ownership epoch metadata key.
pub const OWNERSHIP_EPOCH: &str = "ownership-epoch";

/// Parsed metadata fields used by storage helpers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TypedMetadata {
    /// Highest durable log sequence number.
    pub high_water_lsn: Option<u64>,
    /// Oldest replay log sequence number.
    pub oldest_replay_lsn: Option<u64>,
    /// Relation size in bytes.
    pub relation_size: Option<u64>,
    /// Whether this object marks deletion.
    pub deletion_marker: bool,
    /// Truncation log sequence number.
    pub truncation_lsn: Option<u64>,
    /// Shard configuration payload.
    pub shard_config: Option<String>,
    /// Upload log sequence number.
    pub upload_lsn: Option<u64>,
    /// Backup timestamp or marker.
    pub backup_time: Option<String>,
    /// Ownership epoch.
    pub ownership_epoch: Option<u64>,
}

impl TypedMetadata {
    /// Parses typed metadata from a normalized metadata map.
    pub fn parse(metadata: &MetadataMap) -> Result<Self, Error> {
        Ok(Self {
            high_water_lsn: parse_optional_u64(metadata, HIGH_WATER_LSN)?,
            oldest_replay_lsn: parse_optional_u64(metadata, OLDEST_REPLAY_LSN)?,
            relation_size: parse_optional_u64(metadata, RELATION_SIZE)?,
            deletion_marker: parse_optional_bool(metadata, DELETION_MARKER)?,
            truncation_lsn: parse_optional_u64(metadata, TRUNCATION_LSN)?,
            shard_config: metadata.get(SHARD_CONFIG).map(str::to_owned),
            upload_lsn: parse_optional_u64(metadata, UPLOAD_LSN)?,
            backup_time: metadata.get(BACKUP_TIME).map(str::to_owned),
            ownership_epoch: parse_optional_u64(metadata, OWNERSHIP_EPOCH)?,
        })
    }

    /// Writes present typed fields into a metadata map.
    pub fn write_to(&self, metadata: &mut MetadataMap) {
        insert_optional_u64(metadata, HIGH_WATER_LSN, self.high_water_lsn);
        insert_optional_u64(metadata, OLDEST_REPLAY_LSN, self.oldest_replay_lsn);
        insert_optional_u64(metadata, RELATION_SIZE, self.relation_size);
        if self.deletion_marker {
            metadata.insert(DELETION_MARKER, "true");
        }
        insert_optional_u64(metadata, TRUNCATION_LSN, self.truncation_lsn);
        if let Some(shard_config) = &self.shard_config {
            metadata.insert(SHARD_CONFIG, shard_config);
        }
        insert_optional_u64(metadata, UPLOAD_LSN, self.upload_lsn);
        if let Some(backup_time) = &self.backup_time {
            metadata.insert(BACKUP_TIME, backup_time);
        }
        insert_optional_u64(metadata, OWNERSHIP_EPOCH, self.ownership_epoch);
    }

    /// Creates a new metadata map containing the present typed fields.
    pub fn to_metadata_map(&self) -> MetadataMap {
        let mut metadata = MetadataMap::new();
        self.write_to(&mut metadata);
        metadata
    }
}

fn parse_optional_u64(metadata: &MetadataMap, key: &str) -> Result<Option<u64>, Error> {
    metadata
        .get(key)
        .map(|value| {
            value.parse::<u64>().map_err(|_| {
                Error::new(
                    ErrorKind::MetadataFormat,
                    format!("metadata key {key} is not an unsigned integer"),
                )
            })
        })
        .transpose()
}

fn parse_optional_bool(metadata: &MetadataMap, key: &str) -> Result<bool, Error> {
    match metadata.get(key) {
        None => Ok(false),
        Some("true") | Some("1") => Ok(true),
        Some("false") | Some("0") => Ok(false),
        Some(_) => Err(Error::new(
            ErrorKind::MetadataFormat,
            format!("metadata key {key} is not a boolean"),
        )),
    }
}

fn insert_optional_u64(metadata: &mut MetadataMap, key: &str, value: Option<u64>) {
    if let Some(value) = value {
        metadata.insert(key, value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_metadata_round_trips_known_fields() {
        let typed = TypedMetadata {
            high_water_lsn: Some(42),
            oldest_replay_lsn: Some(7),
            relation_size: Some(4096),
            deletion_marker: true,
            truncation_lsn: Some(30),
            shard_config: Some("shard-a".into()),
            upload_lsn: Some(41),
            backup_time: Some("2026-01-02T03:04:05Z".into()),
            ownership_epoch: Some(9),
        };

        let metadata = typed.to_metadata_map();
        let parsed = TypedMetadata::parse(&metadata).unwrap();

        assert_eq!(parsed, typed);
        assert_eq!(metadata.get("HIGH-WATER-LSN"), Some("42"));
    }

    #[test]
    fn typed_metadata_rejects_malformed_numbers_and_booleans() {
        let mut metadata = MetadataMap::new();
        metadata.insert(HIGH_WATER_LSN, "not-a-number");
        assert_eq!(
            TypedMetadata::parse(&metadata).unwrap_err().kind(),
            ErrorKind::MetadataFormat
        );

        let mut metadata = MetadataMap::new();
        metadata.insert(DELETION_MARKER, "maybe");
        assert_eq!(
            TypedMetadata::parse(&metadata).unwrap_err().kind(),
            ErrorKind::MetadataFormat
        );
    }
}
