// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Low-allocation durable object storage client foundations for `kimojio-stack`.
//!
//! This crate keeps storage operation costs explicit: callers choose auth mode,
//! deadlines, retry policy, body replay behavior, and concurrency limits.

pub mod archive;
pub mod auth;
pub mod backup_status;
pub mod block;
pub mod body;
pub mod compat;
pub mod conditions;
pub mod config_reader;
pub mod container;
pub mod copy;
pub mod diagnostics;
pub mod error;
pub mod list;
pub mod metadata;
pub mod model;
pub mod ownership;
pub mod page;
pub mod pool;
pub mod properties;
pub mod retry;
pub mod routing;
pub mod snapshot;
pub mod snapshot_cleanup;
pub mod tenant_init;
pub mod testing;
pub mod transport;

pub use archive::{
    ArchiveClient, ArchiveDescriptor, Compression, archive_candidates, archive_upload_request,
    crc32, validate_archive_crc,
};
pub use auth::{AuthMode, AuthProvider, AuthRefresh, AuthRefreshContext, KeySecret, SignedSource};
pub use backup_status::{BackupStatus, BackupStatusClient};
pub use block::{
    BlockClient, BlockDelete, BlockUpload, BlockUploadOutcome, DeleteOutcome, block_delete_request,
    block_download_request, block_upload_request,
};
pub use body::{BodyReplay, ReplayBody};
pub use compat::{
    CallerEnvironment, CompatibilityFixtures, ErrorMappingFixture, MetadataFixture, RoutingFixture,
};
pub use conditions::Conditions;
pub use config_reader::{
    ConfigReadOutcome, ConfigReader, ConfigState, config_request, parse_config_json,
};
pub use container::{
    ContainerClient, ContainerCreateOutcome, ContainerDeleteOutcome, container_exists_request,
    create_container_request, delete_container_request,
};
pub use copy::{
    CopyClient, CopyInfo, copy_error_with_diagnostics, copy_from_source_request,
    copy_info_from_response, require_accepted_copy,
};
pub use diagnostics::{AttemptDiagnostics, Diagnostics, OperationClass, RequestCounts};
pub use error::{Error, ErrorKind};
pub use list::{
    ListClient, ListItem, ListOptions, ListPage, is_retryable_list_error, list_request,
    merge_dedup_pages, parse_list_page,
};
pub use metadata::{
    BACKUP_TIME, DELETION_MARKER, HIGH_WATER_LSN, OLDEST_REPLAY_LSN, OWNERSHIP_EPOCH,
    RELATION_SIZE, SHARD_CONFIG, TRUNCATION_LSN, TypedMetadata, UPLOAD_LSN,
};
pub use model::{
    AccountId, ConcurrencyConfig, ContainerName, LeaseContext, MetadataMap, ObjectKind, ObjectName,
    ObjectProperties, ObjectRef, PoolConfig, StorageContext,
};
pub use ownership::{
    LeaseDuration, OwnershipWriteDecision, OwnershipWriteGuard, acquire_ownership_request,
    break_ownership_request, change_ownership_request, renew_ownership_request,
    set_ownership_epoch_request, validate_ownership_epoch,
};
pub use page::{
    PAGE_ALIGNMENT, PageClient, PageRange, PageWriteConditions, PageWriteRecovery,
    SequenceNumberAction, SequenceNumberRecovery, classify_page_write_failure,
    classify_sequence_number_failure, clear_range_request, coalesce_ranges, create_page_request,
    parse_object_properties, parse_written_ranges, properties_request, read_range_request,
    sequence_number_request, upload_range_request, upload_range_request_with_conditions,
    written_ranges_request,
};
pub use pool::{ConcurrencyLimiter, ConcurrencyPermit, IdlePool};
pub use properties::{
    delete_object_request, object_properties_from_headers, object_properties_request,
    set_object_metadata_request,
};
pub use retry::{RetryDecision, RetryObservation, RetryPolicy, RetryState};
pub use routing::{AccountEndpoint, RoutedObject, RoutingTable};
pub use snapshot::{
    DeltaSize, SnapshotClient, SnapshotRef, create_snapshot_request, delete_snapshot_request,
    list_snapshots_request, parse_delta_size, parse_snapshot_properties,
    snapshot_properties_request, snapshot_refs_from_list_items, snapshot_source_reference,
};
pub use snapshot_cleanup::{SnapshotProtection, delete_orphans_bounded, orphan_snapshots};
pub use tenant_init::{
    ContainerTarget, PageObjectInit, TenantInitPlan, TenantInitState, TenantInitializer,
};
pub use testing::{DeterministicEmulator, FakeResponse, FakeService};
pub use transport::{AttemptError, RequestParts, ResponseParts, StackHttpTransport, Transport};
