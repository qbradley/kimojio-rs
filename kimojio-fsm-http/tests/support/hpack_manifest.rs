#![allow(dead_code)]

use super::reference_huffman;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AcceptanceClass {
    Int,
    Huff,
    Rep,
    Table,
    Limit,
    Connection,
    Path,
    Resource,
    Corpus,
    Api,
    Closure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Field {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
    pub sensitive: bool,
}

impl Field {
    fn new(name: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>, sensitive: bool) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            sensitive,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegerForm {
    Canonical,
    NonMinimalValid { zero_groups: u8 },
    Truncated { zero_groups: u8 },
    FirstOverflow,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegerInput {
    pub prefix_bits: u8,
    pub width: u8,
    pub form: IntegerForm,
    pub context: IntegerCodecContext,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegerCodecContext {
    IndexedField,
    IncrementalName,
    SizeUpdate,
    WithoutIndexingName,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HuffmanKind {
    RoundTrip,
    RfcRequest,
    Eos,
    ValidPadding(u8),
    InvalidPadding(u8),
    Truncated(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HuffmanInput {
    pub kind: HuffmanKind,
    pub source: Vec<u8>,
    pub encoded: Vec<u8>,
    pub declared_length: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepresentationCase {
    IndexedStatic,
    IndexedDynamic,
    Incremental,
    WithoutIndexing,
    NeverIndexed,
    SizeUpdate,
    StaticExact,
    DynamicName,
    StaticName,
    DuplicateNewest,
    SensitiveExactDynamic,
    SensitiveExactStatic,
    SensitiveAlternation,
    FittingMiss,
    OversizedMiss,
    EmptyBlock,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepresentationInput {
    pub case: RepresentationCase,
    pub blocks: Vec<Vec<Field>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TablePopulation {
    Empty,
    One,
    MaximumMinimum,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableTransition {
    NewestHit,
    OldestHit,
    Eviction,
    ZeroToNonzero,
    NonzeroToZero,
    MinimumFinalUpdates,
    LeadingUpdate,
    SingleUpdate,
    DuplicateUpdates,
    NoUpdates,
    DecoderMaximum,
    TwoUpdates,
    DescendingSecond,
    RequiredReduction,
    OversizedInsertion,
    RetainedContainerBoundary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableStateOwner {
    EncoderAndDecoder,
    Decoder,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TableAction {
    SetEncoderCapacity(usize),
    SetDecoderCapacity(usize),
    RoundTrip(Vec<Field>),
    RoundTripRepeated { field: Field, count: usize },
    Decode(Vec<u8>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableTransitionInput {
    pub transition: TableTransition,
    pub actions: Vec<TableAction>,
    pub state_owner: TableStateOwner,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitCase {
    DecodedZero,
    DecodedBelow,
    DecodedEqual,
    DecodedOneOver,
    CompactExpansion,
    SynchronizedReuse,
    MalformedTail,
    AccountingOverflow,
    AllocationInbound,
    AllocationOutbound,
    PoisonRepeat,
    NoPartial,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LimitOperation {
    Decode {
        wire: Vec<u8>,
        limit: usize,
    },
    EncodeThenDecode {
        fields: Vec<Field>,
        limit: usize,
    },
    SynchronizedReuse {
        field: Field,
        crossed_limit: usize,
        reuse_limit: usize,
    },
    Accounting {
        name_len: usize,
        value_len: usize,
        limit: usize,
        initial_total: usize,
    },
    PoisonRepeat {
        first_wire: Vec<u8>,
        second_wire: Vec<u8>,
        limit: usize,
    },
    AllocationInbound {
        field: Field,
    },
    AllocationOutbound {
        field: Field,
        capacity_updates: Vec<usize>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LimitInput {
    pub case: LimitCase,
    pub operation: LimitOperation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceOperation {
    Insertion,
    Eviction,
    Resize,
    Clear,
    CapacityOnly,
    CrossedLimitDiscard,
    MalformedInteger,
    MalformedHuffman,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResourceScenario {
    CapacityOnly,
    Insertion {
        field: Field,
    },
    Eviction {
        insertions: usize,
        wire_entry: Vec<u8>,
    },
    Resize {
        field: Field,
        target_capacity: usize,
    },
    Clear {
        field: Field,
        target_capacity: usize,
    },
    CrossedLimitDiscard {
        field: Field,
        limit: usize,
    },
    MalformedInteger {
        continuation_octet: u8,
        continuation_count: usize,
    },
    MalformedHuffman {
        literal: Vec<u8>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceInput {
    pub capacity: usize,
    pub operation: ResourceOperation,
    pub scenario: ResourceScenario,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApiCase {
    RawHeaderType,
    RawHeaderNew,
    RawHeaderAsRef,
    HeaderFieldType,
    HeaderFieldSensitive,
    HeaderFieldAsRef,
    RawHeaderRefType,
    RawHeaderRefNew,
    RawHeaderRefSensitive,
    RawHeaderRefToOwned,
    EncoderType,
    EncoderSetCapacity,
    EncoderEncode,
    EncoderTryEncode,
    EncoderTryEncodeFields,
    EncoderTryEncodeRef,
    DecoderType,
    DecoderSetCapacity,
    DecoderDecodeWithLimit,
    DecoderTryDecodeWithLimit,
    HpackErrorType,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApiInput {
    pub case: ApiCase,
    pub symbol: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CorpusStep {
    pub capacity_updates: Vec<usize>,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CorpusInput {
    pub version: u8,
    pub steps: Vec<CorpusStep>,
    pub baseline_domain_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionEndpoint {
    Client,
    Server,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeaderRole {
    Request,
    Response,
    Trailers,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionOccurrence {
    First,
    Repeated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionBehavior {
    SettingsDecrease,
    SettingsIncrease,
    LocalValidation,
    Cancellation,
    IncompleteInput,
    TerminalFailure,
    EncodedLimitZero,
    EncodedLimitBelow,
    EncodedLimitEqual,
    EncodedLimitOneOver,
    DecodedLimitZero,
    DecodedLimitBelow,
    DecodedLimitEqual,
    DecodedLimitOneOver,
    AllocationInbound,
    AllocationOutboundPreHandoff,
    ResourceTerminalRepeat,
    DiagnosticsSuccess,
    DiagnosticsLimit,
    DiagnosticsError,
    DiagnosticsSaturation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionView {
    Discard,
    FrameAssembly,
    CompleteBlock,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectionScenarioV3 {
    Block {
        endpoint: ConnectionEndpoint,
        role: HeaderRole,
        occurrence: ConnectionOccurrence,
        fields: Vec<Field>,
    },
    Settings {
        behavior: ConnectionBehavior,
        initial_capacity: usize,
        advertised_capacities: Vec<usize>,
        fields: Vec<Field>,
    },
    LocalValidation {
        endpoint: ConnectionEndpoint,
        role: HeaderRole,
        fields: Vec<Field>,
        decoded_limit: usize,
    },
    Cancellation {
        endpoint: ConnectionEndpoint,
        pending_table_size: usize,
        fields: Vec<Field>,
    },
    IncompleteInput {
        endpoint: ConnectionEndpoint,
        stream_id: u32,
        first_fragment: Vec<u8>,
    },
    TerminalFailure {
        endpoint: ConnectionEndpoint,
        first_block: Vec<u8>,
        repeat_block: Vec<u8>,
    },
    EncodedBoundary {
        endpoint: ConnectionEndpoint,
        view: ConnectionView,
        limit: usize,
        block: Vec<u8>,
    },
    EncodedAccountingOverflow {
        endpoint: ConnectionEndpoint,
        stream_id: u32,
        first_fragment: Vec<u8>,
        accounted_len: usize,
        continuation: Vec<u8>,
    },
    DecodedBoundary {
        endpoint: ConnectionEndpoint,
        limit: usize,
        block: Vec<u8>,
    },
    InboundAllocation {
        endpoint: ConnectionEndpoint,
        block: Vec<u8>,
        repeat: bool,
    },
    OutboundAllocation {
        endpoint: ConnectionEndpoint,
        pending_table_size: usize,
        fields: Vec<Field>,
    },
    AssemblyAllocation {
        endpoint: ConnectionEndpoint,
        stream_id: u32,
        first_fragment: Vec<u8>,
        continuation: Vec<u8>,
    },
    Diagnostics {
        behavior: ConnectionBehavior,
        blocks: Vec<Vec<u8>>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathInput {
    Ordinary,
    Sensitive,
    Duplicate,
    NonText,
    Mixed,
    Pseudo,
    PseudoRequest,
    PseudoResponse,
    PseudoTrailers,
    PseudoForward,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathRoleV4 {
    Direct,
    ServerRequest,
    ServerResponse,
    ServerTrailers,
    ClientRequest,
    ClientResponse,
    ClientTrailers,
    TextProjection,
    StackRequest,
    StackResponse,
    StackTrailers,
    FsmGrpcMetadata,
    FsmGrpcStatus,
    StackGrpcMetadata,
    StackGrpcStatus,
    PseudoForward,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathScenarioV4 {
    pub path: u8,
    pub input: PathInput,
    pub fields: Vec<Field>,
    pub roles: Vec<PathRoleV4>,
    pub decoded_binary: Applicability<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CaseInput {
    Integer(IntegerInput),
    Huffman(HuffmanInput),
    Representation(RepresentationInput),
    TableCapacity {
        capacity: usize,
        population: TablePopulation,
    },
    TableTransition(TableTransitionInput),
    Limit(LimitInput),
    Resource(ResourceInput),
    Corpus(CorpusInput),
    Api(ApiInput),
    ConnectionBlock {
        endpoint: ConnectionEndpoint,
        role: HeaderRole,
        occurrence: ConnectionOccurrence,
    },
    ConnectionBehavior(ConnectionBehavior),
    ConnectionV3(ConnectionScenarioV3),
    Path {
        path: u8,
        input: PathInput,
    },
    PathV4(PathScenarioV4),
    Closure(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitialState {
    FreshDefault,
    FreshCapacity(usize),
    ScenarioDefined,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCategory {
    Protocol,
    Integer,
    String,
    InvalidIndex,
    InvalidMaximum,
    InvalidTableUpdate,
    TableUpdateAfterField,
    InvalidHuffman,
    HeaderListTooLarge,
    EncodedHeaderBlockTooLarge,
    FieldSizeOverflow,
    StateOverflow,
    DecoderPoisoned,
    AllocationFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Disposition {
    Reusable,
    Poisoned,
    AllocationTerminal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegerEvidence {
    pub result: Result<u128, ErrorCategory>,
    pub consumed: usize,
    pub codec_outcome: Result<Vec<Field>, ErrorCategory>,
    pub codec_table_capacity: usize,
    pub codec_disposition: Disposition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HuffmanEvidence {
    pub wire: Vec<u8>,
    pub huffman_wire: Option<Vec<u8>>,
    pub encoder_wire: Option<Vec<u8>>,
    pub decoded: Result<Vec<u8>, ErrorCategory>,
    pub disposition: Disposition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepresentationEvidence {
    pub wire_blocks: Vec<Vec<u8>>,
    pub occurrences: Vec<Field>,
    pub table_entries: Vec<Field>,
    pub table_size: usize,
    pub disposition: Disposition,
    pub diagnostics: Option<DiagnosticEvidence>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticEvidence {
    pub encoded_blocks: u64,
    pub decoded_blocks: u64,
    pub indexed_fields: u64,
    pub field_bytes: u64,
    pub wire_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TableEntries {
    Exact(Vec<Field>),
    Repeated { field: Field, count: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableEvidence {
    pub wire_blocks: Vec<Vec<u8>>,
    pub error: Option<ErrorCategory>,
    pub capacity: usize,
    pub entries: TableEntries,
    pub accounted_size: usize,
    pub container_entries_at_most: Option<usize>,
    pub disposition: Disposition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LimitEvidence {
    pub outcome: Result<Vec<Field>, ErrorCategory>,
    pub disposition: Disposition,
    pub table_entries: Vec<Field>,
    pub header_list_too_large_delta: u64,
    pub compression_error_delta: u64,
    pub accounting: Option<AccountingEvidence>,
    pub post_limit_allocations: Applicability<PostLimitAllocationEvidence>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccountingEvidence {
    pub total: usize,
    pub oversized: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Applicability<T> {
    Applicable(T),
    NotApplicable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PostLimitAllocationEvidence {
    pub discarded_output_allocations: usize,
    pub dynamic_table_synchronization_allocations: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceOutcome {
    Success,
    Error(ErrorCategory),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceEvidence {
    pub capacity: usize,
    pub operation: ResourceOperation,
    pub outcome: ResourceOutcome,
    pub final_capacity: usize,
    pub table_entries: usize,
    pub retained_size: usize,
    pub container_entries_at_most: [usize; 3],
    pub minimum_deallocations: usize,
    pub operation_allocations: Applicability<usize>,
    pub post_limit_allocations: Applicability<PostLimitAllocationEvidence>,
    pub disposition: Disposition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectionOutcome {
    Wire(Vec<u8>),
    Error(ErrorCategory),
    Incomplete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Open,
    Closing,
    Terminal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionEvidence {
    pub outcome: ConnectionOutcome,
    pub occurrences: Vec<Field>,
    pub inbound_entries: usize,
    pub outbound_entries: usize,
    pub state: ConnectionState,
    pub diagnostics: Option<DiagnosticEvidence>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectivenessEvidenceV3 {
    Unavailable,
    Exact {
        encoded_wire_octets: u64,
        uncompressed_field_octets: u64,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HpackDiagnosticsEvidenceV3 {
    pub encoded_blocks: u64,
    pub decoded_blocks: u64,
    pub indexed_fields: u64,
    pub incremental_fields: u64,
    pub without_indexing_fields: u64,
    pub never_indexed_fields: u64,
    pub huffman_strings: u64,
    pub plain_strings: u64,
    pub table_size_updates: u64,
    pub table_insertions: u64,
    pub table_evictions: u64,
    pub compression_errors: u64,
    pub local_limit_failures: u64,
    pub field_octets: u64,
    pub wire_octets: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirectionalDiagnosticsEvidenceV3 {
    pub inbound: HpackDiagnosticsEvidenceV3,
    pub outbound: HpackDiagnosticsEvidenceV3,
    pub inbound_effectiveness: EffectivenessEvidenceV3,
    pub outbound_effectiveness: EffectivenessEvidenceV3,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionEvidenceV3 {
    pub outcomes: Vec<ConnectionOutcome>,
    pub occurrences: Vec<Field>,
    pub inbound_entries: usize,
    pub outbound_entries: usize,
    pub state: ConnectionState,
    pub reusable: bool,
    pub commit_sequences: Vec<u64>,
    pub diagnostics: Option<DirectionalDiagnosticsEvidenceV3>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdapterOutcome {
    Preserve(Vec<Field>),
    Reject,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathErrorV4 {
    ValueNotUtf8,
    InvalidPseudoRole,
    PseudoNotForwardable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathRoleEvidenceV4 {
    pub role: PathRoleV4,
    pub outcome: AdapterOutcome,
    pub occurrences: Applicability<Vec<Field>>,
    pub wire: Applicability<Vec<u8>>,
    pub table_entries: Applicability<usize>,
    pub diagnostics: Applicability<HpackDiagnosticsEvidenceV3>,
    pub error: Applicability<PathErrorV4>,
    pub decoded_binary: Applicability<Vec<u8>>,
    pub reusable: Applicability<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathEvidenceV4 {
    pub path: u8,
    pub input: PathInput,
    pub source: Vec<Field>,
    pub source_unchanged: bool,
    pub roles: Vec<PathRoleEvidenceV4>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathEvidence {
    pub path: u8,
    pub input: Vec<Field>,
    pub outcome: AdapterOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExpectedEvidence {
    Integer(IntegerEvidence),
    Huffman(HuffmanEvidence),
    Representation(RepresentationEvidence),
    Table(TableEvidence),
    Limit(LimitEvidence),
    Resource(ResourceEvidence),
    Corpus {
        candidate_wire: Vec<Vec<u8>>,
        baseline_wire: Vec<Vec<u8>>,
        baseline_domain_count: usize,
    },
    Api {
        case: ApiCase,
        symbol: &'static str,
    },
    Connection(ConnectionEvidence),
    ConnectionV3(Box<ConnectionEvidenceV3>),
    Path(PathEvidence),
    PathV4(PathEvidenceV4),
    Closure {
        criterion: u8,
    },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Observable {
    Input,
    InitialState,
    Outcome,
    Wire,
    EncoderWire,
    Occurrences,
    TableState,
    Disposition,
    Diagnostics,
    OperationAllocation,
    DiscardedOutputAllocation,
    DynamicTableSynchronizationAllocation,
    ResourceCapacity,
    ResourceRetention,
    ResourceRelease,
    Aggregate,
    ApiSurface,
    ProjectionError,
    BinaryPayload,
    SourceState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableActualEvidence {
    pub wire_blocks: Vec<Vec<u8>>,
    pub error: Option<ErrorCategory>,
    pub capacity: usize,
    pub entries: TableEntries,
    pub accounted_size: usize,
    pub container_entries: usize,
    pub disposition: Disposition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceActualEvidence {
    pub configured_encoder_capacity: usize,
    pub configured_decoder_capacity: usize,
    pub operation: ResourceOperation,
    pub outcome: ResourceOutcome,
    pub final_capacity: usize,
    pub table_entries: usize,
    pub retained_size: usize,
    pub container_capacities: [usize; 3],
    pub observed_deallocations: usize,
    pub operation_allocations: Applicability<usize>,
    pub post_limit_allocations: Applicability<PostLimitAllocationEvidence>,
    pub disposition: Disposition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActualEvidence {
    Integer(IntegerEvidence),
    Huffman(HuffmanEvidence),
    Representation(RepresentationEvidence),
    Table(TableActualEvidence),
    Limit(LimitEvidence),
    Resource(ResourceActualEvidence),
    Corpus {
        candidate_wire: Vec<Vec<u8>>,
        baseline_wire: Vec<Vec<u8>>,
        baseline_domain_count: usize,
    },
    Api {
        case: ApiCase,
        symbol: &'static str,
    },
    Connection(ConnectionEvidence),
    ConnectionV3(Box<ConnectionEvidenceV3>),
    Path(PathEvidence),
    PathV4(PathEvidenceV4),
    Closure {
        criterion: u8,
    },
}

impl ExpectedEvidence {
    pub fn declares_observable(&self, observable: Observable) -> bool {
        match observable {
            Observable::Outcome => matches!(
                self,
                Self::Integer(_)
                    | Self::Huffman(_)
                    | Self::Table(_)
                    | Self::Limit(_)
                    | Self::Resource(_)
                    | Self::Connection(_)
                    | Self::ConnectionV3(_)
                    | Self::Path(_)
                    | Self::PathV4(_)
            ),
            Observable::Wire => matches!(
                self,
                Self::Huffman(_)
                    | Self::Representation(_)
                    | Self::Table(_)
                    | Self::Corpus { .. }
                    | Self::ConnectionV3(_)
                    | Self::PathV4(_)
            ),
            Observable::Occurrences => matches!(
                self,
                Self::Integer(_)
                    | Self::Representation(_)
                    | Self::Corpus { .. }
                    | Self::ConnectionV3(_)
                    | Self::PathV4(_)
            ),
            Observable::TableState => matches!(
                self,
                Self::Integer(_)
                    | Self::Representation(_)
                    | Self::Table(_)
                    | Self::Limit(_)
                    | Self::Resource(_)
                    | Self::ConnectionV3(_)
                    | Self::PathV4(_)
            ),
            Observable::Disposition => matches!(
                self,
                Self::Integer(_)
                    | Self::Huffman(_)
                    | Self::Representation(_)
                    | Self::Table(_)
                    | Self::Limit(_)
                    | Self::Resource(_)
                    | Self::ConnectionV3(_)
                    | Self::PathV4(_)
            ),
            Observable::Diagnostics => match self {
                Self::Representation(evidence) => evidence.diagnostics.is_some(),
                Self::Limit(_) => true,
                Self::Connection(evidence) => evidence.diagnostics.is_some(),
                Self::ConnectionV3(evidence) => evidence.diagnostics.is_some(),
                Self::PathV4(_) => true,
                _ => false,
            },
            Observable::OperationAllocation => matches!(
                self,
                Self::Resource(ResourceEvidence {
                    operation_allocations: Applicability::Applicable(_),
                    ..
                })
            ),
            Observable::DiscardedOutputAllocation
            | Observable::DynamicTableSynchronizationAllocation => matches!(
                self,
                Self::Limit(LimitEvidence {
                    post_limit_allocations: Applicability::Applicable(_),
                    ..
                }) | Self::Resource(ResourceEvidence {
                    post_limit_allocations: Applicability::Applicable(_),
                    ..
                })
            ),
            Observable::Aggregate => matches!(self, Self::Closure { .. }),
            Observable::EncoderWire
            | Observable::ResourceCapacity
            | Observable::ResourceRetention
            | Observable::ResourceRelease
            | Observable::ApiSurface
            | Observable::Input
            | Observable::InitialState => false,
            Observable::ProjectionError | Observable::BinaryPayload | Observable::SourceState => {
                matches!(self, Self::PathV4(_))
            }
        }
    }

    pub fn assert_matches(&self, actual: &ActualEvidence) -> Vec<Observable> {
        match (self, actual) {
            (Self::Integer(expected), ActualEvidence::Integer(actual)) => {
                assert_eq!(actual, expected);
                vec![
                    Observable::Outcome,
                    Observable::Occurrences,
                    Observable::TableState,
                    Observable::Disposition,
                ]
            }
            (Self::Huffman(expected), ActualEvidence::Huffman(actual)) => {
                assert_eq!(actual, expected);
                vec![
                    Observable::Wire,
                    Observable::EncoderWire,
                    Observable::Outcome,
                    Observable::Disposition,
                ]
            }
            (Self::Representation(expected), ActualEvidence::Representation(actual)) => {
                assert_eq!(actual, expected);
                let mut observables = vec![
                    Observable::Wire,
                    Observable::Occurrences,
                    Observable::TableState,
                    Observable::Disposition,
                ];
                if expected.diagnostics.is_some() {
                    observables.push(Observable::Diagnostics);
                }
                observables
            }
            (Self::Table(expected), ActualEvidence::Table(actual)) => {
                assert_eq!(actual.wire_blocks, expected.wire_blocks);
                assert_eq!(actual.error, expected.error);
                assert_eq!(actual.capacity, expected.capacity);
                assert_eq!(actual.entries, expected.entries);
                assert_eq!(actual.accounted_size, expected.accounted_size);
                assert_eq!(actual.disposition, expected.disposition);
                if let Some(bound) = expected.container_entries_at_most {
                    assert!(
                        actual.container_entries <= bound,
                        "retained container capacity {} exceeds {bound}",
                        actual.container_entries
                    );
                }
                let mut observables = vec![
                    Observable::Wire,
                    Observable::Outcome,
                    Observable::TableState,
                    Observable::Disposition,
                ];
                if expected.container_entries_at_most.is_some() {
                    observables.push(Observable::ResourceRetention);
                }
                observables
            }
            (Self::Limit(expected), ActualEvidence::Limit(actual)) => {
                assert_eq!(actual, expected);
                let mut observables = vec![
                    Observable::Outcome,
                    Observable::TableState,
                    Observable::Disposition,
                    Observable::Diagnostics,
                ];
                if matches!(
                    expected.post_limit_allocations,
                    Applicability::Applicable(_)
                ) {
                    observables.extend([
                        Observable::DiscardedOutputAllocation,
                        Observable::DynamicTableSynchronizationAllocation,
                    ]);
                }
                observables
            }
            (Self::Resource(expected), ActualEvidence::Resource(actual)) => {
                assert_eq!(actual.configured_encoder_capacity, expected.capacity);
                assert_eq!(actual.configured_decoder_capacity, expected.capacity);
                assert_eq!(actual.operation, expected.operation);
                assert_eq!(actual.outcome, expected.outcome);
                assert_eq!(actual.final_capacity, expected.final_capacity);
                assert_eq!(actual.table_entries, expected.table_entries);
                assert_eq!(actual.retained_size, expected.retained_size);
                for (index, (capacity, bound)) in actual
                    .container_capacities
                    .iter()
                    .zip(expected.container_entries_at_most)
                    .enumerate()
                {
                    assert!(
                        *capacity <= bound,
                        "resource container {index} capacity {capacity} exceeds {bound}"
                    );
                }
                assert!(
                    actual.observed_deallocations >= expected.minimum_deallocations,
                    "observed {} deallocations, expected at least {}",
                    actual.observed_deallocations,
                    expected.minimum_deallocations
                );
                assert_eq!(actual.operation_allocations, expected.operation_allocations);
                assert_eq!(
                    actual.post_limit_allocations,
                    expected.post_limit_allocations
                );
                assert_eq!(actual.disposition, expected.disposition);
                let mut observables = vec![
                    Observable::Outcome,
                    Observable::ResourceCapacity,
                    Observable::TableState,
                    Observable::ResourceRetention,
                    Observable::ResourceRelease,
                    Observable::Disposition,
                ];
                if matches!(expected.operation_allocations, Applicability::Applicable(_)) {
                    observables.push(Observable::OperationAllocation);
                }
                if matches!(
                    expected.post_limit_allocations,
                    Applicability::Applicable(_)
                ) {
                    observables.extend([
                        Observable::DiscardedOutputAllocation,
                        Observable::DynamicTableSynchronizationAllocation,
                    ]);
                }
                observables
            }
            (
                Self::Corpus {
                    candidate_wire: expected_candidate,
                    baseline_wire: expected_baseline,
                    baseline_domain_count: expected_domains,
                },
                ActualEvidence::Corpus {
                    candidate_wire: actual_candidate,
                    baseline_wire: actual_baseline,
                    baseline_domain_count: actual_domains,
                },
            ) => {
                assert_eq!(actual_candidate, expected_candidate);
                assert_eq!(actual_baseline, expected_baseline);
                assert_eq!(actual_domains, expected_domains);
                vec![
                    Observable::Wire,
                    Observable::Occurrences,
                    Observable::Aggregate,
                ]
            }
            (
                Self::Api {
                    case: expected_case,
                    symbol: expected_symbol,
                },
                ActualEvidence::Api {
                    case: actual_case,
                    symbol: actual_symbol,
                },
            ) => {
                assert_eq!(actual_case, expected_case);
                assert_eq!(actual_symbol, expected_symbol);
                vec![Observable::ApiSurface]
            }
            (Self::Connection(expected), ActualEvidence::Connection(actual)) => {
                assert_eq!(actual, expected);
                vec![Observable::Outcome]
            }
            (Self::ConnectionV3(expected), ActualEvidence::ConnectionV3(actual)) => {
                assert_eq!(actual, expected);
                let mut observables = vec![
                    Observable::Outcome,
                    Observable::Wire,
                    Observable::Occurrences,
                    Observable::TableState,
                    Observable::Disposition,
                ];
                if expected.diagnostics.is_some() {
                    observables.push(Observable::Diagnostics);
                }
                observables
            }
            (Self::Path(expected), ActualEvidence::Path(actual)) => {
                assert_eq!(actual, expected);
                vec![Observable::Outcome]
            }
            (Self::PathV4(expected), ActualEvidence::PathV4(actual)) => {
                assert_eq!(actual, expected);
                vec![
                    Observable::Outcome,
                    Observable::Wire,
                    Observable::Occurrences,
                    Observable::TableState,
                    Observable::Disposition,
                    Observable::Diagnostics,
                    Observable::ProjectionError,
                    Observable::BinaryPayload,
                    Observable::SourceState,
                ]
            }
            (
                Self::Closure {
                    criterion: expected,
                },
                ActualEvidence::Closure { criterion: actual },
            ) => {
                assert_eq!(actual, expected);
                vec![Observable::Aggregate]
            }
            (expected, actual) => {
                panic!("evidence variant mismatch: expected {expected:?}, actual {actual:?}")
            }
        }
    }
}

const DIAGNOSTIC_OBSERVABLE: &[Observable] = &[Observable::Diagnostics];
const POST_LIMIT_ALLOCATION_OBSERVABLES: &[Observable] = &[
    Observable::DiscardedOutputAllocation,
    Observable::DynamicTableSynchronizationAllocation,
];
const CONNECTION_OUTCOME_OBSERVABLES: &[Observable] = &[
    Observable::Outcome,
    Observable::TableState,
    Observable::Disposition,
];
const CONNECTION_WIRE_OBSERVABLES: &[Observable] = &[
    Observable::Wire,
    Observable::TableState,
    Observable::Disposition,
];
const OCCURRENCE_OBSERVABLE: &[Observable] = &[Observable::Occurrences];
const AGGREGATE_OBSERVABLE: &[Observable] = &[Observable::Aggregate];
const NO_REQUIRED_OBSERVABLES: &[Observable] = &[];

pub fn required_observables(requirement: &str) -> &'static [Observable] {
    if requirement.starts_with("FR-009-") {
        DIAGNOSTIC_OBSERVABLE
    } else if matches!(
        requirement,
        "FR-006-byte-preserving" | "FR-010-adapter-sensitivity"
    ) {
        const PATH_OBSERVABLES: &[Observable] = &[
            Observable::Outcome,
            Observable::Wire,
            Observable::Occurrences,
            Observable::TableState,
            Observable::Disposition,
            Observable::Diagnostics,
            Observable::ProjectionError,
            Observable::BinaryPayload,
            Observable::SourceState,
        ];
        PATH_OBSERVABLES
    } else if requirement == "FR-006-raw-occurrences" {
        OCCURRENCE_OBSERVABLE
    } else if matches!(
        requirement,
        "FR-007-wire-order-handoff" | "FR-008-outbound-cancellation"
    ) {
        CONNECTION_WIRE_OBSERVABLES
    } else if requirement.starts_with("FR-008-") || requirement == "FR-007-directional-connection" {
        CONNECTION_OUTCOME_OBSERVABLES
    } else if requirement == "NFR-001-crossed-limit-no-output-allocation" {
        POST_LIMIT_ALLOCATION_OBSERVABLES
    } else if requirement.starts_with("SC-") {
        AGGREGATE_OBSERVABLE
    } else {
        NO_REQUIRED_OBSERVABLES
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaseDefinition {
    pub id: String,
    pub class: AcceptanceClass,
    pub input: CaseInput,
    pub initial_state: InitialState,
    pub expected: ExpectedEvidence,
    pub requirement_links: Vec<&'static str>,
}

pub const CURRENT_MANIFEST_VERSION: u8 = 4;
pub const MANIFEST_V2_SUPERSEDES_VERSION: u8 = 1;
pub const MANIFEST_V3_SUPERSEDES_VERSION: u8 = 2;
pub const MANIFEST_V4_SUPERSEDES_VERSION: u8 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OracleCorrection {
    pub superseded_id: &'static str,
    pub replacement_id: &'static str,
    pub reason: &'static str,
}

pub const MANIFEST_V2_ORACLE_CORRECTIONS: &[OracleCorrection] = &[
    OracleCorrection {
        superseded_id: "CONNECTION-CLIENT-REQUEST-FIRST-v1",
        replacement_id: "CONNECTION-CLIENT-REQUEST-FIRST-v2",
        reason: "FR-004 requires Huffman coding when it is strictly shorter",
    },
    OracleCorrection {
        superseded_id: "CONNECTION-CLIENT-RESPONSE-FIRST-v1",
        replacement_id: "CONNECTION-CLIENT-RESPONSE-FIRST-v2",
        reason: "FR-004 requires Huffman coding when it is strictly shorter",
    },
    OracleCorrection {
        superseded_id: "CONNECTION-CLIENT-TRAILERS-FIRST-v1",
        replacement_id: "CONNECTION-CLIENT-TRAILERS-FIRST-v2",
        reason: "FR-004 requires Huffman coding when it is strictly shorter",
    },
    OracleCorrection {
        superseded_id: "CONNECTION-SERVER-REQUEST-FIRST-v1",
        replacement_id: "CONNECTION-SERVER-REQUEST-FIRST-v2",
        reason: "FR-004 requires Huffman coding when it is strictly shorter",
    },
    OracleCorrection {
        superseded_id: "CONNECTION-SERVER-RESPONSE-FIRST-v1",
        replacement_id: "CONNECTION-SERVER-RESPONSE-FIRST-v2",
        reason: "FR-004 requires Huffman coding when it is strictly shorter",
    },
    OracleCorrection {
        superseded_id: "CONNECTION-SERVER-TRAILERS-FIRST-v1",
        replacement_id: "CONNECTION-SERVER-TRAILERS-FIRST-v2",
        reason: "FR-004 requires Huffman coding when it is strictly shorter",
    },
];

pub const PHASE_ONE_REQUIREMENTS: &[&str] = &[
    "FR-001-directional-history",
    "FR-002-indexed",
    "FR-002-incremental",
    "FR-002-without-indexing",
    "FR-002-never-indexed",
    "FR-002-leading-size-update",
    "FR-003-accounting",
    "FR-003-newest-index",
    "FR-003-eviction",
    "FR-003-oversized-clear",
    "FR-003-local-ceiling",
    "FR-003-capacity-no-proportional-allocation",
    "FR-003-clear-zero-release",
    "FR-003-resource-failure-local",
    "FR-004-policy-1-sensitive",
    "FR-004-policy-2-static-exact",
    "FR-004-policy-3-dynamic-exact",
    "FR-004-policy-4-name-only",
    "FR-004-policy-5-fitting-miss",
    "FR-004-policy-6-oversized-miss",
    "FR-004-policy-7-huffman-shorter",
    "FR-005-updates-leading",
    "FR-005-minimum-then-final",
    "FR-005-single-update",
    "FR-005-duplicate-collapse",
    "FR-005-no-request-no-update",
    "FR-005-decoder-maximum",
    "FR-005-decoder-two-updates",
    "FR-005-decoder-nondescending",
    "FR-005-required-reduction",
    "FR-008-compression-poison",
    "FR-008-no-partial-delivery",
    "FR-008-local-limit-sync",
    "FR-008-malformed-precedence",
    "FR-008-encoder-rollback",
    "FR-008-limit-accounting",
    "FR-008-limit-equality",
    "FR-008-limit-one-over",
    "FR-008-accounting-overflow",
    "FR-008-decoder-allocation",
    "FR-008-outbound-allocation",
    "FR-009-success-delta",
    "FR-009-local-limit-delta",
    "FR-009-compression-error-delta",
    "NFR-001-bounded-integer",
    "NFR-001-invalid-huffman-no-panic",
    "NFR-001-crossed-limit-no-output-allocation",
    "NFR-002-capacity-no-proportional-allocation",
    "NFR-002-clear-zero-release",
    "NFR-004-versioned-finite-cases",
    "NFR-004-exactly-once",
    "NFR-004-reverse-coverage",
    "ACCEPT-INT",
    "ACCEPT-HUFF",
    "ACCEPT-REP",
    "ACCEPT-TABLE",
    "ACCEPT-LIMIT-CODEC",
    "ACCEPT-RESOURCE",
    "ACCEPT-REPEATED-CORPUS",
    "ACCEPT-PUBLIC-API",
];

pub const PHASE_TWO_REQUIREMENTS: &[&str] = &[
    "FR-001-directional-history",
    "FR-006-raw-occurrences",
    "FR-007-directional-connection",
    "FR-007-wire-order-handoff",
    "FR-008-compression-poison",
    "FR-008-local-limit-sync",
    "FR-008-validation-precedence",
    "FR-008-encoded-limit",
    "FR-008-encoded-accounting-overflow",
    "FR-008-encoded-terminal-repeat",
    "FR-008-decoder-allocation",
    "FR-008-assembly-allocation",
    "FR-008-outbound-allocation",
    "FR-008-outbound-cancellation",
    "FR-009-block-byte-deltas",
    "FR-009-representation-deltas",
    "FR-009-string-deltas",
    "FR-009-table-deltas",
    "FR-009-error-limit-deltas",
    "FR-009-effectiveness",
    "FR-009-atomic-snapshot",
    "FR-009-saturation",
    "FR-009-content-free",
    "ACCEPT-CONNECTION",
    "ACCEPT-LIMIT-CONNECTION",
    "ACCEPT-DIAGNOSTICS",
];

pub const PHASE_THREE_REQUIREMENTS: &[&str] =
    &["FR-006-byte-preserving", "FR-010-adapter-sensitivity"];

pub const PHASE_FOUR_REQUIREMENTS: &[&str] =
    &["SC-001", "SC-002", "SC-003", "SC-004", "SC-005", "SC-006"];

fn encode_integer(mut value: u128, prefix_bits: u8) -> Vec<u8> {
    let prefix_max = (1u128 << prefix_bits) - 1;
    if value < prefix_max {
        return vec![value as u8];
    }
    let mut bytes = vec![prefix_max as u8];
    value -= prefix_max;
    while value >= 128 {
        bytes.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    bytes.push(value as u8);
    bytes
}

fn integer_context(prefix_bits: u8) -> IntegerCodecContext {
    match prefix_bits {
        7 => IntegerCodecContext::IndexedField,
        6 => IntegerCodecContext::IncrementalName,
        5 => IntegerCodecContext::SizeUpdate,
        4 => IntegerCodecContext::WithoutIndexingName,
        _ => unreachable!("unsupported manifest prefix"),
    }
}

fn integer_evidence(input: &IntegerInput, result: Result<u128, ErrorCategory>) -> IntegerEvidence {
    let codec_outcome = match result {
        Err(_) if input.width == 32 && input.form == IntegerForm::FirstOverflow => {
            Err(ErrorCategory::InvalidMaximum)
        }
        Err(_) => Err(ErrorCategory::Integer),
        Ok(value) => match input.context {
            IntegerCodecContext::IndexedField | IntegerCodecContext::IncrementalName => {
                Err(ErrorCategory::InvalidIndex)
            }
            IntegerCodecContext::SizeUpdate if value <= 4096 => Ok(Vec::new()),
            IntegerCodecContext::SizeUpdate => Err(ErrorCategory::InvalidMaximum),
            IntegerCodecContext::WithoutIndexingName => {
                let name = match value {
                    14 => b":status".as_slice(),
                    15 => b"accept-charset".as_slice(),
                    16 => b"accept-encoding".as_slice(),
                    _ => unreachable!("manifest literal-name index"),
                };
                Ok(vec![Field::new(name, Vec::new(), false)])
            }
        },
    };
    IntegerEvidence {
        result,
        consumed: input.bytes.len(),
        codec_table_capacity: match (&codec_outcome, input.context) {
            (Ok(_), IntegerCodecContext::SizeUpdate) => usize::try_from(result.unwrap()).unwrap(),
            _ => 4096,
        },
        codec_disposition: if codec_outcome.is_ok() {
            Disposition::Reusable
        } else {
            Disposition::Poisoned
        },
        codec_outcome,
    }
}

fn push_integer(output: &mut Vec<u8>, mut value: usize, prefix_bits: u8, marker: u8) {
    let prefix_max = (1usize << prefix_bits) - 1;
    if value < prefix_max {
        output.push(marker | value as u8);
        return;
    }
    output.push(marker | prefix_max as u8);
    value -= prefix_max;
    while value >= 128 {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn huffman_literal(encoded: &[u8], declared_length: usize) -> Vec<u8> {
    let mut wire = vec![0x11];
    push_integer(&mut wire, declared_length, 7, 0x80);
    wire.extend_from_slice(encoded);
    wire
}

fn add(
    cases: &mut Vec<CaseDefinition>,
    id: impl Into<String>,
    class: AcceptanceClass,
    input: CaseInput,
    initial_state: InitialState,
    expected: ExpectedEvidence,
    requirement_links: &[&'static str],
) {
    cases.push(CaseDefinition {
        id: id.into(),
        class,
        input,
        initial_state,
        expected,
        requirement_links: requirement_links.to_vec(),
    });
}

fn add_integer_cases(cases: &mut Vec<CaseDefinition>) {
    for prefix_bits in [4, 5, 6, 7] {
        let prefix_max = (1u128 << prefix_bits) - 1;
        for (boundary, value) in [
            ("below", prefix_max - 1),
            ("equal", prefix_max),
            ("one-over", prefix_max + 1),
        ] {
            let bytes = encode_integer(value, prefix_bits);
            let input = IntegerInput {
                prefix_bits,
                width: 64,
                form: IntegerForm::Canonical,
                context: integer_context(prefix_bits),
                bytes,
            };
            add(
                cases,
                format!("INT-P{prefix_bits}-{boundary}-v1"),
                AcceptanceClass::Int,
                CaseInput::Integer(input.clone()),
                InitialState::FreshDefault,
                ExpectedEvidence::Integer(integer_evidence(&input, Ok(value))),
                &["NFR-001-bounded-integer", "ACCEPT-INT"],
            );
        }
    }
    for width in [32u8, 64] {
        for (boundary, value) in [("zero", 0), ("maximum", (1u128 << width) - 1)] {
            let bytes = encode_integer(value, 5);
            let input = IntegerInput {
                prefix_bits: 5,
                width,
                form: IntegerForm::Canonical,
                context: IntegerCodecContext::SizeUpdate,
                bytes,
            };
            add(
                cases,
                format!("INT-W{width}-{boundary}-v1"),
                AcceptanceClass::Int,
                CaseInput::Integer(input.clone()),
                InitialState::FreshDefault,
                ExpectedEvidence::Integer(integer_evidence(&input, Ok(value))),
                &["NFR-001-bounded-integer", "ACCEPT-INT"],
            );
        }
        let maximum_groups = if width == 32 { 5 } else { 10 };
        for groups in 1..=maximum_groups {
            let mut valid = vec![0x1f];
            valid.extend(std::iter::repeat_n(0x80, groups));
            valid.push(0);
            let valid_input = IntegerInput {
                prefix_bits: 5,
                width,
                form: IntegerForm::NonMinimalValid {
                    zero_groups: groups as u8,
                },
                context: IntegerCodecContext::SizeUpdate,
                bytes: valid,
            };
            add(
                cases,
                format!("INT-W{width}-NONMIN-{groups:02}-valid-v1"),
                AcceptanceClass::Int,
                CaseInput::Integer(valid_input.clone()),
                InitialState::FreshDefault,
                ExpectedEvidence::Integer(integer_evidence(&valid_input, Ok(31))),
                &["NFR-001-bounded-integer", "ACCEPT-INT"],
            );
            let mut truncated = valid_input.bytes;
            truncated.pop();
            let truncated_input = IntegerInput {
                prefix_bits: 5,
                width,
                form: IntegerForm::Truncated {
                    zero_groups: groups as u8,
                },
                context: IntegerCodecContext::SizeUpdate,
                bytes: truncated,
            };
            add(
                cases,
                format!("INT-W{width}-NONMIN-{groups:02}-truncated-v1"),
                AcceptanceClass::Int,
                CaseInput::Integer(truncated_input.clone()),
                InitialState::FreshDefault,
                ExpectedEvidence::Integer(integer_evidence(
                    &truncated_input,
                    Err(ErrorCategory::Integer),
                )),
                &["NFR-001-bounded-integer", "ACCEPT-INT"],
            );
        }
        let bytes = encode_integer(1u128 << width, 5);
        let input = IntegerInput {
            prefix_bits: 5,
            width,
            form: IntegerForm::FirstOverflow,
            context: IntegerCodecContext::SizeUpdate,
            bytes,
        };
        add(
            cases,
            format!("INT-W{width}-FIRST-OVERFLOW-v1"),
            AcceptanceClass::Int,
            CaseInput::Integer(input.clone()),
            InitialState::FreshDefault,
            ExpectedEvidence::Integer(integer_evidence(&input, Err(ErrorCategory::Integer))),
            &["NFR-001-bounded-integer", "ACCEPT-INT"],
        );
    }
}

fn add_huffman_round_trip(
    cases: &mut Vec<CaseDefinition>,
    id: String,
    source: Vec<u8>,
    requirements: &[&'static str],
) {
    let encoded = reference_huffman::encode(&source);
    let declared_length = encoded.len();
    let wire = huffman_literal(&encoded, encoded.len());
    add(
        cases,
        id,
        AcceptanceClass::Huff,
        CaseInput::Huffman(HuffmanInput {
            kind: HuffmanKind::RoundTrip,
            source: source.clone(),
            encoded: encoded.clone(),
            declared_length,
        }),
        InitialState::FreshDefault,
        ExpectedEvidence::Huffman(HuffmanEvidence {
            wire,
            huffman_wire: Some(encoded.clone()),
            encoder_wire: Some(if encoded.len() < source.len() {
                huffman_literal(&encoded, encoded.len())
            } else {
                let mut plain = vec![0x11];
                push_integer(&mut plain, source.len(), 7, 0);
                plain.extend_from_slice(&source);
                plain
            }),
            decoded: Ok(source),
            disposition: Disposition::Reusable,
        }),
        requirements,
    );
}

fn add_huffman_cases(cases: &mut Vec<CaseDefinition>) {
    add_huffman_round_trip(
        cases,
        "HUFF-RFC-www-example-com-v1".into(),
        b"www.example.com".to_vec(),
        &["FR-004-policy-7-huffman-shorter", "ACCEPT-HUFF"],
    );
    let request = vec![
        0x82, 0x86, 0x84, 0x41, 0x8c, 0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90,
        0xf4, 0xff,
    ];
    add(
        cases,
        "HUFF-RFC-rfc-request-c4-v1",
        AcceptanceClass::Huff,
        CaseInput::Huffman(HuffmanInput {
            kind: HuffmanKind::RfcRequest,
            source: Vec::new(),
            encoded: request.clone(),
            declared_length: request.len(),
        }),
        InitialState::FreshDefault,
        ExpectedEvidence::Huffman(HuffmanEvidence {
            wire: request,
            huffman_wire: None,
            encoder_wire: None,
            decoded: Ok(
                b":method\0GET\0:scheme\0http\0:path\0/\0:authority\0www.example.com".to_vec(),
            ),
            disposition: Disposition::Reusable,
        }),
        &["FR-004-policy-7-huffman-shorter", "ACCEPT-HUFF"],
    );
    add_huffman_round_trip(
        cases,
        "HUFF-RFC-all-octets-v1".into(),
        (0..=u8::MAX).collect(),
        &["FR-004-policy-7-huffman-shorter", "ACCEPT-HUFF"],
    );
    for octet in 0..=u8::MAX {
        add_huffman_round_trip(
            cases,
            format!("HUFF-OCTET-{octet:02X}-v1"),
            vec![octet],
            &["FR-004-policy-7-huffman-shorter", "ACCEPT-HUFF"],
        );
    }
    for first in 0..=u8::MAX {
        for second in 0..=u8::MAX {
            add_huffman_round_trip(
                cases,
                format!("HUFF-PAIR-{first:02X}-{second:02X}-v1"),
                vec![first, second],
                &["FR-004-policy-7-huffman-shorter", "ACCEPT-HUFF"],
            );
        }
    }

    let eos = vec![0xff, 0xff, 0xff, 0xff];
    add_invalid_huffman(cases, "HUFF-EOS-v1".into(), HuffmanKind::Eos, eos, 4);

    let mut padding_sources = [None; 8];
    'outer: for first in 0..=u8::MAX {
        for second in 0..=u8::MAX {
            let source = [first, second];
            let padding = reference_huffman::padding_bits(&source);
            padding_sources[padding as usize].get_or_insert(source);
            if padding_sources.iter().all(Option::is_some) {
                break 'outer;
            }
        }
    }
    for (padding, source) in padding_sources.into_iter().enumerate() {
        let source = source.unwrap().to_vec();
        let encoded = reference_huffman::encode(&source);
        let declared_length = encoded.len();
        let wire = huffman_literal(&encoded, encoded.len());
        add(
            cases,
            format!("HUFF-PADDING-{padding}-v1"),
            AcceptanceClass::Huff,
            CaseInput::Huffman(HuffmanInput {
                kind: HuffmanKind::ValidPadding(padding as u8),
                source: source.clone(),
                encoded: encoded.clone(),
                declared_length,
            }),
            InitialState::FreshDefault,
            ExpectedEvidence::Huffman(HuffmanEvidence {
                wire,
                huffman_wire: Some(encoded.clone()),
                encoder_wire: Some(if encoded.len() < source.len() {
                    huffman_literal(&encoded, encoded.len())
                } else {
                    let mut plain = vec![0x11];
                    push_integer(&mut plain, source.len(), 7, 0);
                    plain.extend_from_slice(&source);
                    plain
                }),
                decoded: Ok(source),
                disposition: Disposition::Reusable,
            }),
            &["NFR-001-invalid-huffman-no-panic", "ACCEPT-HUFF"],
        );
        if padding != 0 {
            let mut invalid =
                reference_huffman::encode(padding_sources[padding].as_ref().unwrap().as_slice());
            *invalid.last_mut().unwrap() &= !1;
            add_invalid_huffman(
                cases,
                format!("HUFF-INVALID-PADDING-{padding}-v1"),
                HuffmanKind::InvalidPadding(padding as u8),
                invalid.clone(),
                invalid.len(),
            );
        }
    }
    add_invalid_huffman(
        cases,
        "HUFF-INVALID-PADDING-8-v1".into(),
        HuffmanKind::InvalidPadding(8),
        vec![0xff],
        1,
    );

    let www = reference_huffman::encode(b"www.example.com");
    for boundary in 0..www.len() {
        add_invalid_huffman(
            cases,
            format!("HUFF-TRUNC-{boundary:02}-v1"),
            HuffmanKind::Truncated(boundary),
            www[..boundary].to_vec(),
            www.len(),
        );
    }
}

fn add_invalid_huffman(
    cases: &mut Vec<CaseDefinition>,
    id: String,
    kind: HuffmanKind,
    encoded: Vec<u8>,
    declared_length: usize,
) {
    let wire = huffman_literal(&encoded, declared_length);
    add(
        cases,
        id,
        AcceptanceClass::Huff,
        CaseInput::Huffman(HuffmanInput {
            kind,
            source: Vec::new(),
            encoded,
            declared_length,
        }),
        InitialState::FreshDefault,
        ExpectedEvidence::Huffman(HuffmanEvidence {
            wire,
            huffman_wire: None,
            encoder_wire: None,
            decoded: Err(if matches!(kind, HuffmanKind::Truncated(_)) {
                ErrorCategory::String
            } else {
                ErrorCategory::InvalidHuffman
            }),
            disposition: Disposition::Poisoned,
        }),
        &["NFR-001-invalid-huffman-no-panic", "ACCEPT-HUFF"],
    );
}

fn rep_evidence(case: RepresentationCase) -> RepresentationEvidence {
    let x_a = Field::new(b"x", b"a", false);
    let x_b = Field::new(b"x", b"b", false);
    let x_a_sensitive = Field::new(b"x", b"a", true);
    let method = Field::new(b":method", b"GET", false);
    let method_sensitive = Field::new(b":method", b"GET", true);
    let literal_x_a = vec![0x40, 1, b'x', 1, b'a'];
    match case {
        RepresentationCase::IndexedStatic | RepresentationCase::StaticExact => {
            RepresentationEvidence {
                wire_blocks: vec![vec![0x82]],
                occurrences: vec![method],
                table_entries: vec![],
                table_size: 0,
                disposition: Disposition::Reusable,
                diagnostics: (case == RepresentationCase::IndexedStatic).then_some(
                    DiagnosticEvidence {
                        encoded_blocks: 1,
                        decoded_blocks: 1,
                        indexed_fields: 1,
                        field_bytes: 10,
                        wire_bytes: 1,
                    },
                ),
            }
        }
        RepresentationCase::IndexedDynamic => RepresentationEvidence {
            wire_blocks: vec![literal_x_a, vec![0xbe]],
            occurrences: vec![x_a.clone(), x_a.clone()],
            table_entries: vec![x_a],
            table_size: 34,
            disposition: Disposition::Reusable,
            diagnostics: None,
        },
        RepresentationCase::Incremental | RepresentationCase::FittingMiss => {
            RepresentationEvidence {
                wire_blocks: vec![literal_x_a],
                occurrences: vec![x_a.clone()],
                table_entries: vec![x_a],
                table_size: 34,
                disposition: Disposition::Reusable,
                diagnostics: None,
            }
        }
        RepresentationCase::WithoutIndexing | RepresentationCase::OversizedMiss => {
            RepresentationEvidence {
                wire_blocks: vec![vec![0x20, 0x00, 1, b'x', 1, b'a']],
                occurrences: vec![x_a],
                table_entries: vec![],
                table_size: 0,
                disposition: Disposition::Reusable,
                diagnostics: None,
            }
        }
        RepresentationCase::NeverIndexed => RepresentationEvidence {
            wire_blocks: vec![vec![0x10, 1, b'x', 1, b'a']],
            occurrences: vec![x_a_sensitive],
            table_entries: vec![],
            table_size: 0,
            disposition: Disposition::Reusable,
            diagnostics: None,
        },
        RepresentationCase::SizeUpdate => RepresentationEvidence {
            wire_blocks: vec![vec![0x20]],
            occurrences: vec![],
            table_entries: vec![],
            table_size: 0,
            disposition: Disposition::Reusable,
            diagnostics: None,
        },
        RepresentationCase::DynamicName => RepresentationEvidence {
            wire_blocks: vec![literal_x_a, vec![0x7e, 1, b'b']],
            occurrences: vec![x_a.clone(), x_b.clone()],
            table_entries: vec![x_b, x_a],
            table_size: 68,
            disposition: Disposition::Reusable,
            diagnostics: None,
        },
        RepresentationCase::StaticName => {
            let field = Field::new(b"content-type", b"b", false);
            RepresentationEvidence {
                wire_blocks: vec![vec![0x5f, 1, b'b']],
                occurrences: vec![field.clone()],
                table_entries: vec![field],
                table_size: 45,
                disposition: Disposition::Reusable,
                diagnostics: None,
            }
        }
        RepresentationCase::DuplicateNewest => RepresentationEvidence {
            wire_blocks: vec![literal_x_a, vec![0xbe], vec![0x7e, 1, b'b']],
            occurrences: vec![x_a.clone(), x_a.clone(), x_b.clone()],
            table_entries: vec![x_b, x_a],
            table_size: 68,
            disposition: Disposition::Reusable,
            diagnostics: None,
        },
        RepresentationCase::SensitiveExactDynamic => RepresentationEvidence {
            wire_blocks: vec![literal_x_a, vec![0x1f, 0x2f, 1, b'a']],
            occurrences: vec![x_a.clone(), x_a_sensitive],
            table_entries: vec![x_a],
            table_size: 34,
            disposition: Disposition::Reusable,
            diagnostics: None,
        },
        RepresentationCase::SensitiveExactStatic => RepresentationEvidence {
            wire_blocks: vec![vec![0x12, 3, b'G', b'E', b'T']],
            occurrences: vec![method_sensitive],
            table_entries: vec![],
            table_size: 0,
            disposition: Disposition::Reusable,
            diagnostics: None,
        },
        RepresentationCase::SensitiveAlternation => RepresentationEvidence {
            wire_blocks: vec![literal_x_a, vec![0x1f, 0x2f, 1, b'a'], vec![0xbe]],
            occurrences: vec![x_a.clone(), x_a_sensitive, x_a.clone()],
            table_entries: vec![x_a],
            table_size: 34,
            disposition: Disposition::Reusable,
            diagnostics: None,
        },
        RepresentationCase::EmptyBlock => RepresentationEvidence {
            wire_blocks: vec![vec![]],
            occurrences: vec![],
            table_entries: vec![],
            table_size: 0,
            disposition: Disposition::Reusable,
            diagnostics: None,
        },
    }
}

fn representation_input(case: RepresentationCase) -> RepresentationInput {
    let ordinary = |value: &[u8]| Field::new(b"x", value, false);
    let sensitive = || Field::new(b"x", b"a", true);
    let blocks = match case {
        RepresentationCase::IndexedStatic | RepresentationCase::StaticExact => {
            vec![vec![Field::new(b":method", b"GET", false)]]
        }
        RepresentationCase::IndexedDynamic => {
            vec![vec![ordinary(b"a")], vec![ordinary(b"a")]]
        }
        RepresentationCase::Incremental
        | RepresentationCase::WithoutIndexing
        | RepresentationCase::FittingMiss
        | RepresentationCase::OversizedMiss => vec![vec![ordinary(b"a")]],
        RepresentationCase::NeverIndexed => vec![vec![sensitive()]],
        RepresentationCase::SizeUpdate | RepresentationCase::EmptyBlock => vec![Vec::new()],
        RepresentationCase::DynamicName => {
            vec![vec![ordinary(b"a")], vec![ordinary(b"b")]]
        }
        RepresentationCase::StaticName => {
            vec![vec![Field::new(b"content-type", b"b", false)]]
        }
        RepresentationCase::DuplicateNewest => vec![
            vec![ordinary(b"a")],
            vec![ordinary(b"a")],
            vec![ordinary(b"b")],
        ],
        RepresentationCase::SensitiveExactDynamic => {
            vec![vec![ordinary(b"a")], vec![sensitive()]]
        }
        RepresentationCase::SensitiveExactStatic => {
            vec![vec![Field::new(b":method", b"GET", true)]]
        }
        RepresentationCase::SensitiveAlternation => vec![
            vec![ordinary(b"a")],
            vec![sensitive()],
            vec![ordinary(b"a")],
        ],
    };
    RepresentationInput { case, blocks }
}

fn table_transition_input(transition: TableTransition) -> TableTransitionInput {
    let field = |value: &[u8]| Field::new(b"x", value, false);
    let (actions, state_owner) = match transition {
        TableTransition::NewestHit => (
            vec![
                TableAction::RoundTrip(vec![field(b"a")]),
                TableAction::RoundTrip(vec![field(b"a")]),
            ],
            TableStateOwner::EncoderAndDecoder,
        ),
        TableTransition::OldestHit => (
            vec![
                TableAction::RoundTrip(vec![field(b"a")]),
                TableAction::RoundTrip(vec![field(b"b")]),
                TableAction::RoundTrip(vec![field(b"a")]),
            ],
            TableStateOwner::EncoderAndDecoder,
        ),
        TableTransition::Eviction => (
            vec![
                TableAction::SetEncoderCapacity(34),
                TableAction::SetDecoderCapacity(34),
                TableAction::RoundTrip(vec![field(b"a")]),
                TableAction::RoundTrip(vec![field(b"b")]),
            ],
            TableStateOwner::EncoderAndDecoder,
        ),
        TableTransition::ZeroToNonzero => (
            vec![
                TableAction::SetEncoderCapacity(0),
                TableAction::SetDecoderCapacity(0),
                TableAction::RoundTrip(Vec::new()),
                TableAction::SetEncoderCapacity(128),
                TableAction::SetDecoderCapacity(128),
                TableAction::RoundTrip(vec![field(b"a")]),
            ],
            TableStateOwner::EncoderAndDecoder,
        ),
        TableTransition::NonzeroToZero => (
            vec![
                TableAction::RoundTrip(vec![field(b"a")]),
                TableAction::SetEncoderCapacity(0),
                TableAction::SetDecoderCapacity(0),
                TableAction::RoundTrip(Vec::new()),
            ],
            TableStateOwner::EncoderAndDecoder,
        ),
        TableTransition::MinimumFinalUpdates => (
            vec![
                TableAction::SetEncoderCapacity(0),
                TableAction::SetEncoderCapacity(128),
                TableAction::SetDecoderCapacity(128),
                TableAction::RoundTrip(Vec::new()),
            ],
            TableStateOwner::EncoderAndDecoder,
        ),
        TableTransition::LeadingUpdate => (
            vec![
                TableAction::SetEncoderCapacity(0),
                TableAction::SetDecoderCapacity(0),
                TableAction::RoundTrip(vec![field(b"a")]),
            ],
            TableStateOwner::EncoderAndDecoder,
        ),
        TableTransition::SingleUpdate => (
            vec![
                TableAction::SetEncoderCapacity(128),
                TableAction::SetDecoderCapacity(128),
                TableAction::RoundTrip(Vec::new()),
            ],
            TableStateOwner::EncoderAndDecoder,
        ),
        TableTransition::DuplicateUpdates => (
            vec![
                TableAction::SetEncoderCapacity(128),
                TableAction::SetEncoderCapacity(128),
                TableAction::SetDecoderCapacity(128),
                TableAction::RoundTrip(Vec::new()),
            ],
            TableStateOwner::EncoderAndDecoder,
        ),
        TableTransition::NoUpdates => (
            vec![TableAction::RoundTrip(Vec::new())],
            TableStateOwner::EncoderAndDecoder,
        ),
        TableTransition::DecoderMaximum => {
            let mut wire = Vec::new();
            push_integer(&mut wire, 129, 5, 0x20);
            (
                vec![
                    TableAction::SetDecoderCapacity(128),
                    TableAction::Decode(wire),
                ],
                TableStateOwner::Decoder,
            )
        }
        TableTransition::TwoUpdates => (
            vec![
                TableAction::SetDecoderCapacity(128),
                TableAction::Decode(vec![0x20, 0x3f, 0x61]),
            ],
            TableStateOwner::Decoder,
        ),
        TableTransition::DescendingSecond => {
            let mut wire = Vec::new();
            push_integer(&mut wire, 128, 5, 0x20);
            push_integer(&mut wire, 64, 5, 0x20);
            (
                vec![
                    TableAction::SetDecoderCapacity(128),
                    TableAction::Decode(wire),
                ],
                TableStateOwner::Decoder,
            )
        }
        TableTransition::RequiredReduction => (
            vec![
                TableAction::SetDecoderCapacity(0),
                TableAction::Decode(Vec::new()),
            ],
            TableStateOwner::Decoder,
        ),
        TableTransition::OversizedInsertion => (
            vec![
                TableAction::SetEncoderCapacity(32),
                TableAction::SetDecoderCapacity(32),
                TableAction::RoundTrip(vec![field(b"a")]),
            ],
            TableStateOwner::EncoderAndDecoder,
        ),
        TableTransition::RetainedContainerBoundary => (
            vec![
                TableAction::SetEncoderCapacity(64),
                TableAction::SetDecoderCapacity(64),
                TableAction::RoundTrip(vec![field(b"a")]),
                TableAction::RoundTripRepeated {
                    field: field(b"a"),
                    count: 4096,
                },
            ],
            TableStateOwner::EncoderAndDecoder,
        ),
    };
    TableTransitionInput {
        transition,
        actions,
        state_owner,
    }
}

fn table_transition_wire(transition: TableTransition) -> (Vec<Vec<u8>>, Option<ErrorCategory>) {
    let literal_a = vec![0x40, 1, b'x', 1, b'a'];
    let literal_b_dynamic_name = vec![0x7e, 1, b'b'];
    let mut update_34 = Vec::new();
    push_integer(&mut update_34, 34, 5, 0x20);
    update_34.extend_from_slice(&literal_a);
    let mut update_128 = Vec::new();
    push_integer(&mut update_128, 128, 5, 0x20);
    let mut update_32_literal = Vec::new();
    push_integer(&mut update_32_literal, 32, 5, 0x20);
    update_32_literal.extend_from_slice(&[0x00, 1, b'x', 1, b'a']);
    let mut update_64_literal = Vec::new();
    push_integer(&mut update_64_literal, 64, 5, 0x20);
    update_64_literal.extend_from_slice(&literal_a);

    match transition {
        TableTransition::NewestHit => (vec![literal_a, vec![0xbe]], None),
        TableTransition::OldestHit => (vec![literal_a, literal_b_dynamic_name, vec![0xbf]], None),
        TableTransition::Eviction => (vec![update_34, literal_b_dynamic_name], None),
        TableTransition::ZeroToNonzero => {
            let mut final_block = update_128;
            final_block.extend_from_slice(&literal_a);
            (vec![vec![0x20], final_block], None)
        }
        TableTransition::NonzeroToZero => (vec![literal_a, vec![0x20]], None),
        TableTransition::MinimumFinalUpdates => (vec![vec![0x20, 0x3f, 0x61]], None),
        TableTransition::LeadingUpdate => (vec![vec![0x20, 0x00, 1, b'x', 1, b'a']], None),
        TableTransition::SingleUpdate | TableTransition::DuplicateUpdates => {
            (vec![update_128], None)
        }
        TableTransition::NoUpdates | TableTransition::RequiredReduction => (
            vec![Vec::new()],
            (transition == TableTransition::RequiredReduction)
                .then_some(ErrorCategory::InvalidTableUpdate),
        ),
        TableTransition::DecoderMaximum => {
            let mut wire = Vec::new();
            push_integer(&mut wire, 129, 5, 0x20);
            (vec![wire], Some(ErrorCategory::InvalidMaximum))
        }
        TableTransition::TwoUpdates => (vec![vec![0x20, 0x3f, 0x61]], None),
        TableTransition::DescendingSecond => {
            let mut wire = Vec::new();
            push_integer(&mut wire, 128, 5, 0x20);
            push_integer(&mut wire, 64, 5, 0x20);
            (vec![wire], Some(ErrorCategory::InvalidTableUpdate))
        }
        TableTransition::OversizedInsertion => (vec![update_32_literal], None),
        TableTransition::RetainedContainerBoundary => {
            (vec![update_64_literal, vec![0xbe; 4096]], None)
        }
    }
}

fn limit_input(case: LimitCase) -> LimitInput {
    let operation = match case {
        LimitCase::DecodedZero => LimitOperation::Decode {
            wire: vec![0x82],
            limit: 0,
        },
        LimitCase::DecodedBelow | LimitCase::DecodedOneOver => LimitOperation::Decode {
            wire: vec![0x82],
            limit: 41,
        },
        LimitCase::DecodedEqual => LimitOperation::Decode {
            wire: vec![0x82],
            limit: 42,
        },
        LimitCase::CompactExpansion => LimitOperation::EncodeThenDecode {
            fields: vec![Field::new(b"x", vec![b'v'; 8192], false)],
            limit: 1,
        },
        LimitCase::SynchronizedReuse => LimitOperation::SynchronizedReuse {
            field: Field::new(b"x-limit-state", b"value", false),
            crossed_limit: 0,
            reuse_limit: usize::MAX,
        },
        LimitCase::MalformedTail => LimitOperation::Decode {
            wire: vec![0x40, 1, b'x', 1, b'v', 0x80],
            limit: 0,
        },
        LimitCase::AccountingOverflow => LimitOperation::Accounting {
            name_len: 1,
            value_len: 0,
            limit: usize::MAX,
            initial_total: usize::MAX - 1,
        },
        LimitCase::AllocationInbound => LimitOperation::AllocationInbound {
            field: Field::new(b"x-allocation", b"value", false),
        },
        LimitCase::AllocationOutbound => LimitOperation::AllocationOutbound {
            field: Field::new(b"x-allocation", b"value", false),
            capacity_updates: vec![0, 128],
        },
        LimitCase::PoisonRepeat => LimitOperation::PoisonRepeat {
            first_wire: vec![0x80],
            second_wire: vec![0x82],
            limit: usize::MAX,
        },
        LimitCase::NoPartial => LimitOperation::Decode {
            wire: vec![0x82, 0x80],
            limit: usize::MAX,
        },
    };
    LimitInput { case, operation }
}

fn resource_input(capacity: usize, operation: ResourceOperation) -> ResourceInput {
    let field = || Field::new(b"x", b"v", false);
    let scenario = match operation {
        ResourceOperation::CapacityOnly => ResourceScenario::CapacityOnly,
        ResourceOperation::Insertion => ResourceScenario::Insertion { field: field() },
        ResourceOperation::Eviction => ResourceScenario::Eviction {
            insertions: capacity / 32 + usize::from(capacity != 0),
            wire_entry: vec![0x40, 0, 0],
        },
        ResourceOperation::Resize => ResourceScenario::Resize {
            field: field(),
            target_capacity: 0,
        },
        ResourceOperation::Clear => ResourceScenario::Clear {
            field: field(),
            target_capacity: 0,
        },
        ResourceOperation::CrossedLimitDiscard => ResourceScenario::CrossedLimitDiscard {
            field: field(),
            limit: 0,
        },
        ResourceOperation::MalformedInteger => ResourceScenario::MalformedInteger {
            continuation_octet: 0xff,
            continuation_count: 16,
        },
        ResourceOperation::MalformedHuffman => ResourceScenario::MalformedHuffman {
            literal: vec![0x10, 0x01, b'x', 0x84, 0xff, 0xff, 0xff, 0xff],
        },
    };
    ResourceInput {
        capacity,
        operation,
        scenario,
    }
}

fn limit_post_limit_allocations(case: LimitCase) -> Applicability<PostLimitAllocationEvidence> {
    let dynamic_table_synchronization_allocations = match case {
        LimitCase::SynchronizedReuse | LimitCase::MalformedTail => 4,
        LimitCase::DecodedZero
        | LimitCase::DecodedBelow
        | LimitCase::DecodedOneOver
        | LimitCase::CompactExpansion => 0,
        LimitCase::DecodedEqual
        | LimitCase::AccountingOverflow
        | LimitCase::AllocationInbound
        | LimitCase::AllocationOutbound
        | LimitCase::PoisonRepeat
        | LimitCase::NoPartial => return Applicability::NotApplicable,
    };
    Applicability::Applicable(PostLimitAllocationEvidence {
        discarded_output_allocations: 0,
        dynamic_table_synchronization_allocations,
    })
}

fn resource_outcome(operation: ResourceOperation) -> ResourceOutcome {
    match operation {
        ResourceOperation::CrossedLimitDiscard => {
            ResourceOutcome::Error(ErrorCategory::HeaderListTooLarge)
        }
        ResourceOperation::MalformedInteger => ResourceOutcome::Error(ErrorCategory::Integer),
        ResourceOperation::MalformedHuffman => {
            ResourceOutcome::Error(ErrorCategory::InvalidHuffman)
        }
        ResourceOperation::Insertion
        | ResourceOperation::Eviction
        | ResourceOperation::Resize
        | ResourceOperation::Clear
        | ResourceOperation::CapacityOnly => ResourceOutcome::Success,
    }
}

fn resource_post_limit_allocations(
    capacity: usize,
    operation: ResourceOperation,
) -> Applicability<PostLimitAllocationEvidence> {
    if operation != ResourceOperation::CrossedLimitDiscard {
        return Applicability::NotApplicable;
    }
    Applicability::Applicable(PostLimitAllocationEvidence {
        discarded_output_allocations: 0,
        dynamic_table_synchronization_allocations: if capacity >= 34 { 4 } else { 0 },
    })
}

fn corpus_v1_input() -> CorpusInput {
    CorpusInput {
        version: 1,
        steps: vec![
            CorpusStep {
                capacity_updates: Vec::new(),
                fields: vec![Field::new(b":method", b"GET", false)],
            },
            CorpusStep {
                capacity_updates: Vec::new(),
                fields: vec![Field::new(b"x-repeated", b"v1", false)],
            },
            CorpusStep {
                capacity_updates: Vec::new(),
                fields: vec![Field::new(b"x-repeated", b"v1", false)],
            },
            CorpusStep {
                capacity_updates: Vec::new(),
                fields: vec![Field::new(b"x-repeated", b"v2", false)],
            },
            CorpusStep {
                capacity_updates: Vec::new(),
                fields: vec![
                    Field::new(b"x-duplicate", b"value", false),
                    Field::new(b"x-duplicate", b"value", false),
                ],
            },
            CorpusStep {
                capacity_updates: vec![0, 256],
                fields: vec![Field::new(b"x-capacity", b"value", false)],
            },
            CorpusStep {
                capacity_updates: Vec::new(),
                fields: vec![Field::new(b"authorization", b"secret", true)],
            },
            CorpusStep {
                capacity_updates: Vec::new(),
                fields: vec![Field::new(b"authorization", b"secret", true)],
            },
        ],
        baseline_domain_count: 7,
    }
}

fn baseline_corpus_wire(input: &CorpusInput) -> Vec<Vec<u8>> {
    input
        .steps
        .iter()
        .map(|step| {
            let mut wire = Vec::new();
            if let Some(minimum) = step.capacity_updates.iter().copied().min() {
                push_integer(&mut wire, minimum, 5, 0x20);
                let final_size = *step.capacity_updates.last().unwrap();
                if final_size != minimum {
                    push_integer(&mut wire, final_size, 5, 0x20);
                }
            }
            for field in &step.fields {
                wire.push(if field.sensitive { 0x10 } else { 0x00 });
                push_integer(&mut wire, field.name.len(), 7, 0);
                wire.extend_from_slice(&field.name);
                push_integer(&mut wire, field.value.len(), 7, 0);
                wire.extend_from_slice(&field.value);
            }
            wire
        })
        .collect()
}

fn add_codec_cases(cases: &mut Vec<CaseDefinition>) {
    let representations = [
        (
            RepresentationCase::IndexedStatic,
            "INDEXED-STATIC",
            "FR-002-indexed",
        ),
        (
            RepresentationCase::IndexedDynamic,
            "INDEXED-DYNAMIC",
            "FR-004-policy-3-dynamic-exact",
        ),
        (
            RepresentationCase::Incremental,
            "INCREMENTAL",
            "FR-002-incremental",
        ),
        (
            RepresentationCase::WithoutIndexing,
            "WITHOUT",
            "FR-002-without-indexing",
        ),
        (
            RepresentationCase::NeverIndexed,
            "NEVER",
            "FR-002-never-indexed",
        ),
        (
            RepresentationCase::SizeUpdate,
            "SIZE-UPDATE",
            "FR-002-leading-size-update",
        ),
        (
            RepresentationCase::StaticExact,
            "STATIC-EXACT",
            "FR-004-policy-2-static-exact",
        ),
        (
            RepresentationCase::DynamicName,
            "DYNAMIC-NAME",
            "FR-004-policy-4-name-only",
        ),
        (
            RepresentationCase::StaticName,
            "STATIC-NAME",
            "FR-004-policy-4-name-only",
        ),
        (
            RepresentationCase::DuplicateNewest,
            "DUPLICATE-NEWEST",
            "FR-003-newest-index",
        ),
        (
            RepresentationCase::SensitiveExactDynamic,
            "SENSITIVE-EXACT-DYNAMIC",
            "FR-004-policy-1-sensitive",
        ),
        (
            RepresentationCase::SensitiveExactStatic,
            "SENSITIVE-EXACT-STATIC",
            "FR-004-policy-1-sensitive",
        ),
        (
            RepresentationCase::SensitiveAlternation,
            "SENSITIVE-ALTERNATION",
            "FR-004-policy-1-sensitive",
        ),
        (
            RepresentationCase::FittingMiss,
            "FITTING-MISS",
            "FR-004-policy-5-fitting-miss",
        ),
        (
            RepresentationCase::OversizedMiss,
            "OVERSIZED-MISS",
            "FR-004-policy-6-oversized-miss",
        ),
        (
            RepresentationCase::EmptyBlock,
            "EMPTY-BLOCK",
            "FR-001-directional-history",
        ),
    ];
    for (case, name, requirement) in representations {
        let mut links = vec![requirement, "ACCEPT-REP"];
        if case == RepresentationCase::IndexedStatic {
            links.push("FR-009-success-delta");
        }
        add(
            cases,
            format!("REP-{name}-v1"),
            AcceptanceClass::Rep,
            CaseInput::Representation(representation_input(case)),
            if matches!(
                case,
                RepresentationCase::WithoutIndexing
                    | RepresentationCase::OversizedMiss
                    | RepresentationCase::SizeUpdate
            ) {
                InitialState::FreshCapacity(0)
            } else {
                InitialState::FreshDefault
            },
            ExpectedEvidence::Representation(rep_evidence(case)),
            &links,
        );
    }

    for capacity in [0, 4096, 65_535, 65_536, 65_537, 1_048_576] {
        for (population, name) in [
            (TablePopulation::Empty, "EMPTY"),
            (TablePopulation::One, "ONE"),
            (TablePopulation::MaximumMinimum, "MAX-MINIMUM"),
        ] {
            let requested: usize = match population {
                TablePopulation::Empty => 0,
                TablePopulation::One => 1,
                TablePopulation::MaximumMinimum => capacity / 32,
            };
            let entry_count = requested.min(capacity / 32);
            let mut wire = Vec::with_capacity(requested.saturating_mul(3).saturating_add(8));
            push_integer(&mut wire, capacity, 5, 0x20);
            for _ in 0..requested {
                wire.extend_from_slice(&[0x40, 0, 0]);
            }
            add(
                cases,
                format!("TABLE-CAP-{capacity}-{name}-v1"),
                AcceptanceClass::Table,
                CaseInput::TableCapacity {
                    capacity,
                    population,
                },
                InitialState::FreshCapacity(capacity),
                ExpectedEvidence::Table(TableEvidence {
                    wire_blocks: vec![wire],
                    error: None,
                    capacity,
                    entries: if entry_count > 1 {
                        TableEntries::Repeated {
                            field: Field::new(Vec::new(), Vec::new(), false),
                            count: entry_count,
                        }
                    } else {
                        TableEntries::Exact(
                            (0..entry_count)
                                .map(|_| Field::new(Vec::new(), Vec::new(), false))
                                .collect(),
                        )
                    },
                    accounted_size: entry_count * 32,
                    container_entries_at_most: None,
                    disposition: Disposition::Reusable,
                }),
                &["FR-003-accounting", "FR-003-local-ceiling", "ACCEPT-TABLE"],
            );
        }
    }

    let transitions = [
        (
            TableTransition::NewestHit,
            "NEWEST-HIT",
            "FR-003-newest-index",
        ),
        (
            TableTransition::OldestHit,
            "OLDEST-HIT",
            "FR-003-newest-index",
        ),
        (TableTransition::Eviction, "EVICTION", "FR-003-eviction"),
        (
            TableTransition::ZeroToNonzero,
            "ZERO-TO-NONZERO",
            "FR-003-newest-index",
        ),
        (
            TableTransition::NonzeroToZero,
            "NONZERO-TO-ZERO",
            "FR-003-clear-zero-release",
        ),
        (
            TableTransition::MinimumFinalUpdates,
            "MIN-FINAL-UPDATES",
            "FR-005-minimum-then-final",
        ),
        (
            TableTransition::LeadingUpdate,
            "LEADING-UPDATE",
            "FR-005-updates-leading",
        ),
        (
            TableTransition::SingleUpdate,
            "SINGLE-UPDATE",
            "FR-005-single-update",
        ),
        (
            TableTransition::DuplicateUpdates,
            "DUPLICATE-UPDATES",
            "FR-005-duplicate-collapse",
        ),
        (
            TableTransition::NoUpdates,
            "NO-UPDATES",
            "FR-005-no-request-no-update",
        ),
        (
            TableTransition::DecoderMaximum,
            "DECODER-MAXIMUM",
            "FR-005-decoder-maximum",
        ),
        (
            TableTransition::TwoUpdates,
            "TWO-UPDATES",
            "FR-005-decoder-two-updates",
        ),
        (
            TableTransition::DescendingSecond,
            "DESCENDING-SECOND",
            "FR-005-decoder-nondescending",
        ),
        (
            TableTransition::RequiredReduction,
            "REQUIRED-REDUCTION",
            "FR-005-required-reduction",
        ),
        (
            TableTransition::OversizedInsertion,
            "OVERSIZED-INSERTION",
            "FR-003-oversized-clear",
        ),
        (
            TableTransition::RetainedContainerBoundary,
            "RETAINED-CONTAINER-BOUNDARY",
            "NFR-002-capacity-no-proportional-allocation",
        ),
    ];
    for (transition, name, requirement) in transitions {
        let mut links = vec![requirement, "ACCEPT-TABLE"];
        if transition == TableTransition::NonzeroToZero {
            links.push("NFR-002-clear-zero-release");
        }
        add(
            cases,
            format!("TABLE-{name}-v1"),
            AcceptanceClass::Table,
            CaseInput::TableTransition(table_transition_input(transition)),
            InitialState::FreshDefault,
            ExpectedEvidence::Table({
                let (wire_blocks, error) = table_transition_wire(transition);
                TableEvidence {
                    wire_blocks,
                    error,
                    capacity: match transition {
                        TableTransition::ZeroToNonzero => 128,
                        TableTransition::NonzeroToZero => 0,
                        TableTransition::MinimumFinalUpdates
                        | TableTransition::SingleUpdate
                        | TableTransition::DuplicateUpdates
                        | TableTransition::TwoUpdates
                        | TableTransition::DescendingSecond => 128,
                        TableTransition::LeadingUpdate => 0,
                        TableTransition::Eviction => 34,
                        TableTransition::OversizedInsertion => 32,
                        TableTransition::RetainedContainerBoundary => 64,
                        _ => 4096,
                    },
                    entries: match transition {
                        TableTransition::NonzeroToZero
                        | TableTransition::MinimumFinalUpdates
                        | TableTransition::LeadingUpdate
                        | TableTransition::SingleUpdate
                        | TableTransition::DuplicateUpdates
                        | TableTransition::NoUpdates
                        | TableTransition::DecoderMaximum
                        | TableTransition::TwoUpdates
                        | TableTransition::DescendingSecond
                        | TableTransition::RequiredReduction
                        | TableTransition::OversizedInsertion => TableEntries::Exact(Vec::new()),
                        TableTransition::OldestHit => TableEntries::Exact(vec![
                            Field::new(b"x", b"b", false),
                            Field::new(b"x", b"a", false),
                        ]),
                        TableTransition::Eviction => {
                            TableEntries::Exact(vec![Field::new(b"x", b"b", false)])
                        }
                        _ => TableEntries::Exact(vec![Field::new(b"x", b"a", false)]),
                    },
                    accounted_size: match transition {
                        TableTransition::NonzeroToZero
                        | TableTransition::MinimumFinalUpdates
                        | TableTransition::LeadingUpdate
                        | TableTransition::SingleUpdate
                        | TableTransition::DuplicateUpdates
                        | TableTransition::NoUpdates
                        | TableTransition::DecoderMaximum
                        | TableTransition::TwoUpdates
                        | TableTransition::DescendingSecond
                        | TableTransition::RequiredReduction => 0,
                        TableTransition::OldestHit => 68,
                        TableTransition::OversizedInsertion => 0,
                        _ => 34,
                    },
                    container_entries_at_most: (transition
                        == TableTransition::RetainedContainerBoundary)
                        .then_some(4),
                    disposition: if matches!(
                        transition,
                        TableTransition::DecoderMaximum
                            | TableTransition::DescendingSecond
                            | TableTransition::RequiredReduction
                    ) {
                        Disposition::Poisoned
                    } else {
                        Disposition::Reusable
                    },
                }
            }),
            &links,
        );
    }

    let limits = [
        (
            LimitCase::DecodedZero,
            "DECODED-ZERO",
            "FR-008-limit-accounting",
        ),
        (
            LimitCase::DecodedBelow,
            "DECODED-BELOW",
            "FR-008-limit-one-over",
        ),
        (
            LimitCase::DecodedEqual,
            "DECODED-EQUAL",
            "FR-008-limit-equality",
        ),
        (
            LimitCase::DecodedOneOver,
            "DECODED-ONE-OVER",
            "FR-008-limit-one-over",
        ),
        (
            LimitCase::CompactExpansion,
            "COMPACT-EXPANSION",
            "NFR-001-crossed-limit-no-output-allocation",
        ),
        (
            LimitCase::SynchronizedReuse,
            "SYNC-REUSE",
            "FR-008-local-limit-sync",
        ),
        (
            LimitCase::MalformedTail,
            "MALFORMED-TAIL",
            "FR-008-malformed-precedence",
        ),
        (
            LimitCase::AccountingOverflow,
            "ACCOUNTING-OVERFLOW",
            "FR-008-accounting-overflow",
        ),
        (
            LimitCase::AllocationInbound,
            "ALLOC-INBOUND",
            "FR-008-decoder-allocation",
        ),
        (
            LimitCase::AllocationOutbound,
            "ALLOC-OUTBOUND",
            "FR-008-outbound-allocation",
        ),
        (
            LimitCase::PoisonRepeat,
            "POISON-REPEAT",
            "FR-008-compression-poison",
        ),
        (
            LimitCase::NoPartial,
            "NO-PARTIAL",
            "FR-008-no-partial-delivery",
        ),
    ];
    for (limit, name, requirement) in limits {
        let outcome = match limit {
            LimitCase::DecodedEqual => Ok(vec![Field::new(b":method", b"GET", false)]),
            LimitCase::SynchronizedReuse => Ok(vec![Field::new(b"x-limit-state", b"value", false)]),
            LimitCase::AllocationInbound | LimitCase::AllocationOutbound => {
                Err(ErrorCategory::AllocationFailed)
            }
            LimitCase::MalformedTail | LimitCase::PoisonRepeat | LimitCase::NoPartial => {
                Err(ErrorCategory::InvalidIndex)
            }
            LimitCase::AccountingOverflow
            | LimitCase::DecodedZero
            | LimitCase::DecodedBelow
            | LimitCase::DecodedOneOver
            | LimitCase::CompactExpansion => Err(ErrorCategory::HeaderListTooLarge),
        };
        let disposition = match limit {
            LimitCase::MalformedTail | LimitCase::PoisonRepeat | LimitCase::NoPartial => {
                Disposition::Poisoned
            }
            LimitCase::AllocationInbound => Disposition::AllocationTerminal,
            _ => Disposition::Reusable,
        };
        let mut links = vec![requirement, "ACCEPT-LIMIT-CODEC"];
        match limit {
            LimitCase::DecodedOneOver => links.push("FR-009-local-limit-delta"),
            LimitCase::MalformedTail => links.push("FR-009-compression-error-delta"),
            LimitCase::AllocationOutbound => {
                links.extend(["FR-008-encoder-rollback", "FR-003-resource-failure-local"])
            }
            LimitCase::AllocationInbound => links.push("FR-003-resource-failure-local"),
            _ => {}
        }
        add(
            cases,
            format!("LIMIT-CODEC-{name}-v1"),
            AcceptanceClass::Limit,
            CaseInput::Limit(limit_input(limit)),
            InitialState::FreshDefault,
            ExpectedEvidence::Limit(LimitEvidence {
                outcome,
                disposition,
                table_entries: match limit {
                    LimitCase::SynchronizedReuse => {
                        vec![Field::new(b"x-limit-state", b"value", false)]
                    }
                    LimitCase::MalformedTail => vec![Field::new(b"x", b"v", false)],
                    _ => Vec::new(),
                },
                header_list_too_large_delta: u64::from(matches!(
                    limit,
                    LimitCase::DecodedZero
                        | LimitCase::DecodedBelow
                        | LimitCase::DecodedOneOver
                        | LimitCase::CompactExpansion
                        | LimitCase::SynchronizedReuse
                )),
                compression_error_delta: u64::from(matches!(
                    limit,
                    LimitCase::MalformedTail | LimitCase::PoisonRepeat | LimitCase::NoPartial
                )),
                accounting: (limit == LimitCase::AccountingOverflow).then_some(
                    AccountingEvidence {
                        total: usize::MAX,
                        oversized: true,
                    },
                ),
                post_limit_allocations: limit_post_limit_allocations(limit),
            }),
            &links,
        );
    }

    for capacity in [0, 4096, 65_535, 65_536, 65_537, 1_048_576] {
        for (operation, name) in [
            (ResourceOperation::Insertion, "INSERTION"),
            (ResourceOperation::Eviction, "EVICTION"),
            (ResourceOperation::Resize, "RESIZE"),
            (ResourceOperation::Clear, "CLEAR"),
            (ResourceOperation::CapacityOnly, "CAPACITY-ONLY"),
            (
                ResourceOperation::CrossedLimitDiscard,
                "CROSSED-LIMIT-DISCARD",
            ),
            (ResourceOperation::MalformedInteger, "MALFORMED-INTEGER"),
            (ResourceOperation::MalformedHuffman, "MALFORMED-HUFFMAN"),
        ] {
            let entry_count = match operation {
                ResourceOperation::Insertion | ResourceOperation::CrossedLimitDiscard => {
                    usize::from(capacity >= 34)
                }
                ResourceOperation::Eviction => capacity / 32,
                _ => 0,
            };
            let mut links = vec![
                "FR-003-capacity-no-proportional-allocation",
                "NFR-002-capacity-no-proportional-allocation",
                "ACCEPT-RESOURCE",
            ];
            match operation {
                ResourceOperation::CrossedLimitDiscard => {
                    links.push("NFR-001-crossed-limit-no-output-allocation");
                }
                ResourceOperation::MalformedInteger => {
                    links.push("NFR-001-bounded-integer");
                }
                ResourceOperation::MalformedHuffman => {
                    links.push("NFR-001-invalid-huffman-no-panic");
                }
                ResourceOperation::Clear => {
                    links.extend(["FR-003-clear-zero-release", "NFR-002-clear-zero-release"]);
                }
                _ => {}
            }
            add(
                cases,
                format!("RESOURCE-{capacity}-{name}-v1"),
                AcceptanceClass::Resource,
                CaseInput::Resource(resource_input(capacity, operation)),
                InitialState::FreshCapacity(capacity),
                ExpectedEvidence::Resource(ResourceEvidence {
                    capacity,
                    operation,
                    outcome: resource_outcome(operation),
                    final_capacity: if matches!(
                        operation,
                        ResourceOperation::Resize | ResourceOperation::Clear
                    ) {
                        0
                    } else {
                        capacity
                    },
                    table_entries: entry_count,
                    retained_size: match operation {
                        ResourceOperation::Insertion | ResourceOperation::CrossedLimitDiscard
                            if capacity >= 34 =>
                        {
                            34
                        }
                        ResourceOperation::Eviction => entry_count * 32,
                        _ => 0,
                    },
                    container_entries_at_most: if matches!(
                        operation,
                        ResourceOperation::Resize | ResourceOperation::Clear
                    ) {
                        [0; 3]
                    } else {
                        [capacity / 32 + 4, capacity / 16 + 8, capacity / 16 + 8]
                    },
                    minimum_deallocations: usize::from(
                        operation == ResourceOperation::Clear && capacity != 0,
                    ),
                    operation_allocations: if operation == ResourceOperation::CapacityOnly {
                        Applicability::Applicable(1)
                    } else {
                        Applicability::NotApplicable
                    },
                    post_limit_allocations: resource_post_limit_allocations(capacity, operation),
                    disposition: if matches!(
                        operation,
                        ResourceOperation::MalformedInteger | ResourceOperation::MalformedHuffman
                    ) {
                        Disposition::Poisoned
                    } else {
                        Disposition::Reusable
                    },
                }),
                &links,
            );
        }
    }

    let corpus = corpus_v1_input();
    add(
        cases,
        "CORPUS-REPEATED-FIELDS-v1",
        AcceptanceClass::Corpus,
        CaseInput::Corpus(corpus.clone()),
        InitialState::FreshDefault,
        ExpectedEvidence::Corpus {
            candidate_wire: vec![
                vec![0x82],
                vec![
                    0x40, 0x87, 0xf2, 0xb5, 0x85, 0xac, 0xa3, 0x49, 0x64, 0x02, b'v', b'1',
                ],
                vec![0xbe],
                vec![0x7e, 0x02, b'v', b'2'],
                vec![
                    0x40, 0x88, 0xf2, 0xb4, 0x96, 0xd7, 0x41, 0x88, 0x34, 0x97, 0x84, 0xee, 0x3a,
                    0x2d, 0x2f, 0xbe,
                ],
                vec![
                    0x20, 0x3f, 0xe1, 0x01, 0x40, 0x87, 0xf2, 0xb1, 0x07, 0x58, 0xc8, 0x64, 0xfa,
                    0x84, 0xee, 0x3a, 0x2d, 0x2f,
                ],
                vec![0x1f, 0x08, 0x84, 0x41, 0x49, 0x61, 0x53],
                vec![0x1f, 0x08, 0x84, 0x41, 0x49, 0x61, 0x53],
            ],
            baseline_wire: baseline_corpus_wire(&corpus),
            baseline_domain_count: corpus.baseline_domain_count,
        },
        &[
            "ACCEPT-REPEATED-CORPUS",
            "FR-001-directional-history",
            "NFR-004-versioned-finite-cases",
            "NFR-004-exactly-once",
            "NFR-004-reverse-coverage",
        ],
    );

    for (case, symbol) in [
        (ApiCase::RawHeaderType, "H2RawHeader"),
        (ApiCase::RawHeaderNew, "H2RawHeader::new"),
        (ApiCase::RawHeaderAsRef, "H2RawHeader::as_ref"),
        (ApiCase::HeaderFieldType, "H2HeaderField"),
        (
            ApiCase::HeaderFieldSensitive,
            "H2HeaderField::with_sensitive",
        ),
        (ApiCase::HeaderFieldAsRef, "H2HeaderField::as_ref"),
        (ApiCase::RawHeaderRefType, "H2RawHeaderRef"),
        (ApiCase::RawHeaderRefNew, "H2RawHeaderRef::new"),
        (
            ApiCase::RawHeaderRefSensitive,
            "H2RawHeaderRef::with_sensitive",
        ),
        (ApiCase::RawHeaderRefToOwned, "H2RawHeaderRef::to_owned"),
        (ApiCase::EncoderType, "H2HeaderBlockEncoder"),
        (
            ApiCase::EncoderSetCapacity,
            "H2HeaderBlockEncoder::set_max_table_size",
        ),
        (ApiCase::EncoderEncode, "H2HeaderBlockEncoder::encode"),
        (
            ApiCase::EncoderTryEncode,
            "H2HeaderBlockEncoder::try_encode",
        ),
        (
            ApiCase::EncoderTryEncodeFields,
            "H2HeaderBlockEncoder::try_encode_fields",
        ),
        (
            ApiCase::EncoderTryEncodeRef,
            "H2HeaderBlockEncoder::try_encode_ref",
        ),
        (ApiCase::DecoderType, "H2HeaderBlockDecoder"),
        (
            ApiCase::DecoderSetCapacity,
            "H2HeaderBlockDecoder::set_max_table_size",
        ),
        (
            ApiCase::DecoderDecodeWithLimit,
            "H2HeaderBlockDecoder::decode_with_limit",
        ),
        (
            ApiCase::DecoderTryDecodeWithLimit,
            "H2HeaderBlockDecoder::try_decode_with_limit",
        ),
        (ApiCase::HpackErrorType, "H2HpackError"),
    ] {
        add(
            cases,
            format!(
                "API-{}-v1",
                symbol
                    .replace("::", "-")
                    .replace('_', "-")
                    .to_ascii_uppercase()
            ),
            AcceptanceClass::Api,
            CaseInput::Api(ApiInput { case, symbol }),
            InitialState::FreshDefault,
            ExpectedEvidence::Api { case, symbol },
            &["ACCEPT-PUBLIC-API"],
        );
    }
}

fn add_later_phase_cases(cases: &mut Vec<CaseDefinition>) {
    for (endpoint, endpoint_name) in [
        (ConnectionEndpoint::Client, "CLIENT"),
        (ConnectionEndpoint::Server, "SERVER"),
    ] {
        for (role, role_name) in [
            (HeaderRole::Request, "REQUEST"),
            (HeaderRole::Response, "RESPONSE"),
            (HeaderRole::Trailers, "TRAILERS"),
        ] {
            for (occurrence, occurrence_name) in [
                (ConnectionOccurrence::First, "FIRST"),
                (ConnectionOccurrence::Repeated, "REPEATED"),
            ] {
                let input = CaseInput::ConnectionBlock {
                    endpoint,
                    role,
                    occurrence,
                };
                let field = Field::new(b"x-manifest", b"value", false);
                add(
                    cases,
                    format!("CONNECTION-{endpoint_name}-{role_name}-{occurrence_name}-v1"),
                    AcceptanceClass::Connection,
                    input.clone(),
                    InitialState::ScenarioDefined,
                    ExpectedEvidence::Connection(ConnectionEvidence {
                        outcome: ConnectionOutcome::Wire(
                            if occurrence == ConnectionOccurrence::First {
                                vec![
                                    0x40, 0x0a, b'x', b'-', b'm', b'a', b'n', b'i', b'f', b'e',
                                    b's', b't', 0x05, b'v', b'a', b'l', b'u', b'e',
                                ]
                            } else {
                                vec![0xbe]
                            },
                        ),
                        occurrences: vec![field],
                        inbound_entries: usize::from(endpoint == ConnectionEndpoint::Server),
                        outbound_entries: usize::from(endpoint == ConnectionEndpoint::Client),
                        state: ConnectionState::Open,
                        diagnostics: None,
                    }),
                    &[
                        "FR-001-directional-history",
                        "FR-007-directional-connection",
                        "ACCEPT-CONNECTION",
                    ],
                );
            }
        }
    }
    let behaviors = [
        (ConnectionBehavior::SettingsDecrease, "SETTINGS-DECREASE"),
        (ConnectionBehavior::SettingsIncrease, "SETTINGS-INCREASE"),
        (ConnectionBehavior::LocalValidation, "LOCAL-VALIDATION"),
        (ConnectionBehavior::Cancellation, "CANCELLATION"),
        (ConnectionBehavior::IncompleteInput, "INCOMPLETE-INPUT"),
        (ConnectionBehavior::TerminalFailure, "TERMINAL-FAILURE"),
        (ConnectionBehavior::EncodedLimitZero, "ENCODED-LIMIT-ZERO"),
        (ConnectionBehavior::EncodedLimitBelow, "ENCODED-LIMIT-BELOW"),
        (ConnectionBehavior::EncodedLimitEqual, "ENCODED-LIMIT-EQUAL"),
        (
            ConnectionBehavior::EncodedLimitOneOver,
            "ENCODED-LIMIT-ONE-OVER",
        ),
        (ConnectionBehavior::DecodedLimitZero, "DECODED-LIMIT-ZERO"),
        (ConnectionBehavior::DecodedLimitBelow, "DECODED-LIMIT-BELOW"),
        (ConnectionBehavior::DecodedLimitEqual, "DECODED-LIMIT-EQUAL"),
        (
            ConnectionBehavior::DecodedLimitOneOver,
            "DECODED-LIMIT-ONE-OVER",
        ),
        (ConnectionBehavior::AllocationInbound, "ALLOC-INBOUND"),
        (
            ConnectionBehavior::AllocationOutboundPreHandoff,
            "ALLOC-OUTBOUND-PRE-HANDOFF",
        ),
        (
            ConnectionBehavior::ResourceTerminalRepeat,
            "RESOURCE-TERMINAL-REPEAT",
        ),
        (
            ConnectionBehavior::DiagnosticsSuccess,
            "DIAGNOSTICS-SUCCESS",
        ),
        (ConnectionBehavior::DiagnosticsLimit, "DIAGNOSTICS-LIMIT"),
        (ConnectionBehavior::DiagnosticsError, "DIAGNOSTICS-ERROR"),
        (
            ConnectionBehavior::DiagnosticsSaturation,
            "DIAGNOSTICS-SATURATION",
        ),
    ];
    for (behavior, name) in behaviors {
        let input = CaseInput::ConnectionBehavior(behavior);
        let (outcome, state, diagnostics) = match behavior {
            ConnectionBehavior::SettingsDecrease => (
                ConnectionOutcome::Wire(vec![0x20]),
                ConnectionState::Open,
                None,
            ),
            ConnectionBehavior::SettingsIncrease => (
                ConnectionOutcome::Wire(vec![0x3f, 0x61]),
                ConnectionState::Open,
                None,
            ),
            ConnectionBehavior::Cancellation | ConnectionBehavior::IncompleteInput => {
                (ConnectionOutcome::Incomplete, ConnectionState::Open, None)
            }
            ConnectionBehavior::TerminalFailure => (
                ConnectionOutcome::Error(ErrorCategory::InvalidIndex),
                ConnectionState::Terminal,
                None,
            ),
            ConnectionBehavior::AllocationInbound
            | ConnectionBehavior::AllocationOutboundPreHandoff
            | ConnectionBehavior::ResourceTerminalRepeat => (
                ConnectionOutcome::Error(ErrorCategory::AllocationFailed),
                ConnectionState::Terminal,
                None,
            ),
            ConnectionBehavior::EncodedLimitZero
            | ConnectionBehavior::EncodedLimitBelow
            | ConnectionBehavior::EncodedLimitOneOver
            | ConnectionBehavior::DecodedLimitZero
            | ConnectionBehavior::DecodedLimitBelow
            | ConnectionBehavior::DecodedLimitOneOver => (
                ConnectionOutcome::Error(ErrorCategory::HeaderListTooLarge),
                ConnectionState::Closing,
                None,
            ),
            ConnectionBehavior::EncodedLimitEqual
            | ConnectionBehavior::DecodedLimitEqual
            | ConnectionBehavior::LocalValidation => (
                ConnectionOutcome::Wire(Vec::new()),
                ConnectionState::Open,
                None,
            ),
            ConnectionBehavior::DiagnosticsSuccess => (
                ConnectionOutcome::Wire(vec![0x82]),
                ConnectionState::Open,
                Some(DiagnosticEvidence {
                    encoded_blocks: 1,
                    decoded_blocks: 1,
                    indexed_fields: 1,
                    field_bytes: 10,
                    wire_bytes: 1,
                }),
            ),
            ConnectionBehavior::DiagnosticsLimit => (
                ConnectionOutcome::Error(ErrorCategory::HeaderListTooLarge),
                ConnectionState::Open,
                Some(DiagnosticEvidence {
                    encoded_blocks: 0,
                    decoded_blocks: 1,
                    indexed_fields: 1,
                    field_bytes: 10,
                    wire_bytes: 1,
                }),
            ),
            ConnectionBehavior::DiagnosticsError => (
                ConnectionOutcome::Error(ErrorCategory::InvalidIndex),
                ConnectionState::Terminal,
                Some(DiagnosticEvidence {
                    encoded_blocks: 0,
                    decoded_blocks: 0,
                    indexed_fields: 0,
                    field_bytes: 0,
                    wire_bytes: 0,
                }),
            ),
            ConnectionBehavior::DiagnosticsSaturation => (
                ConnectionOutcome::Wire(Vec::new()),
                ConnectionState::Open,
                Some(DiagnosticEvidence {
                    encoded_blocks: u64::MAX,
                    decoded_blocks: u64::MAX,
                    indexed_fields: u64::MAX,
                    field_bytes: u64::MAX,
                    wire_bytes: u64::MAX,
                }),
            ),
        };
        add(
            cases,
            format!("CONNECTION-{name}-v1"),
            AcceptanceClass::Connection,
            input.clone(),
            InitialState::ScenarioDefined,
            ExpectedEvidence::Connection(ConnectionEvidence {
                outcome,
                occurrences: Vec::new(),
                inbound_entries: 0,
                outbound_entries: 0,
                state,
                diagnostics,
            }),
            &["FR-007-directional-connection", "ACCEPT-CONNECTION"],
        );
    }

    for path in 1..=7 {
        for (input, name) in [
            (PathInput::Ordinary, "ORDINARY"),
            (PathInput::Sensitive, "SENSITIVE"),
            (PathInput::Duplicate, "DUPLICATE"),
            (PathInput::NonText, "NON-TEXT"),
        ] {
            let fields = match input {
                PathInput::Ordinary => vec![Field::new(b"x-path", b"value", false)],
                PathInput::Sensitive => {
                    vec![Field::new(b"authorization", b"secret", true)]
                }
                PathInput::Duplicate => vec![
                    Field::new(b"x-duplicate", b"one", false),
                    Field::new(b"x-duplicate", b"two", false),
                ],
                PathInput::NonText => vec![Field::new(b"x-bytes", [0, 0x80, 0xff], false)],
                PathInput::Mixed
                | PathInput::Pseudo
                | PathInput::PseudoRequest
                | PathInput::PseudoResponse
                | PathInput::PseudoTrailers
                | PathInput::PseudoForward => unreachable!(),
            };
            add(
                cases,
                format!("PATH-{path:02}-{name}-v1"),
                AcceptanceClass::Path,
                CaseInput::Path { path, input },
                InitialState::ScenarioDefined,
                ExpectedEvidence::Path(PathEvidence {
                    path,
                    input: fields.clone(),
                    outcome: AdapterOutcome::Preserve(fields),
                }),
                &["FR-006-byte-preserving", "FR-010-adapter-sensitivity"],
            );
        }
    }
    for path in 2..=5 {
        add(
            cases,
            format!("PATH-{path:02}-PSEUDO-v1"),
            AcceptanceClass::Path,
            CaseInput::Path {
                path,
                input: PathInput::Pseudo,
            },
            InitialState::ScenarioDefined,
            ExpectedEvidence::Path(PathEvidence {
                path,
                input: vec![Field::new(b":method", b"GET", false)],
                outcome: AdapterOutcome::Preserve(vec![Field::new(b":method", b"GET", false)]),
            }),
            &["FR-006-byte-preserving", "FR-010-adapter-sensitivity"],
        );
    }
    add(
        cases,
        "PATH-08-PSEUDO-FORWARD-v1",
        AcceptanceClass::Path,
        CaseInput::Path {
            path: 8,
            input: PathInput::PseudoForward,
        },
        InitialState::ScenarioDefined,
        ExpectedEvidence::Path(PathEvidence {
            path: 8,
            input: vec![Field::new(b":method", b"GET", false)],
            outcome: AdapterOutcome::Reject,
        }),
        &["FR-010-adapter-sensitivity"],
    );
    for criterion in 1..=6 {
        add(
            cases,
            format!("CLOSURE-SC-{criterion:03}-v1"),
            AcceptanceClass::Closure,
            CaseInput::Closure(criterion),
            InitialState::ScenarioDefined,
            ExpectedEvidence::Closure { criterion },
            &[match criterion {
                1 => "SC-001",
                2 => "SC-002",
                3 => "SC-003",
                4 => "SC-004",
                5 => "SC-005",
                6 => "SC-006",
                _ => unreachable!(),
            }],
        );
    }
}

pub fn manifest_v1() -> Vec<CaseDefinition> {
    let mut cases = Vec::new();
    add_integer_cases(&mut cases);
    add_huffman_cases(&mut cases);
    add_codec_cases(&mut cases);
    add_later_phase_cases(&mut cases);
    cases
}

pub fn manifest_v2() -> Vec<CaseDefinition> {
    manifest_v1()
        .into_iter()
        .map(|mut case| {
            let superseded_id = case.id.clone();
            case.id = format!(
                "{}-v2",
                superseded_id
                    .strip_suffix("-v1")
                    .expect("Manifest v1 case ID must end in -v1")
            );
            if let Some(correction) = MANIFEST_V2_ORACLE_CORRECTIONS
                .iter()
                .find(|correction| correction.superseded_id == superseded_id)
            {
                assert_eq!(case.id, correction.replacement_id);
                let ExpectedEvidence::Connection(evidence) = &mut case.expected else {
                    panic!("corrected Manifest v2 row must contain connection evidence")
                };
                evidence.outcome = ConnectionOutcome::Wire(vec![
                    0x40, 0x87, 0xf2, 0xb5, 0x23, 0xa8, 0xd2, 0x95, 0x09, 0x84, 0xee, 0x3a, 0x2d,
                    0x2f,
                ]);
            }
            case
        })
        .collect()
}

fn connection_role_fields_v3(role: HeaderRole) -> Vec<Field> {
    let field = Field::new(b"x-manifest", b"value", false);
    match role {
        HeaderRole::Request => vec![
            Field::new(b":method", b"GET", false),
            Field::new(b":scheme", b"https", false),
            Field::new(b":authority", b"example.test", false),
            Field::new(b":path", b"/", false),
            field,
        ],
        HeaderRole::Response => vec![Field::new(b":status", b"200", false), field],
        HeaderRole::Trailers => vec![field],
    }
}

fn connection_role_wire_v3(role: HeaderRole, occurrence: ConnectionOccurrence) -> Vec<u8> {
    match (role, occurrence) {
        (HeaderRole::Request, ConnectionOccurrence::First) => vec![
            0x82, 0x87, 0x41, 0x89, 0x2f, 0x91, 0xd3, 0x5d, 0x05, 0x5d, 0x25, 0x42, 0x7f, 0x84,
            0x40, 0x87, 0xf2, 0xb5, 0x23, 0xa8, 0xd2, 0x95, 0x09, 0x84, 0xee, 0x3a, 0x2d, 0x2f,
        ],
        (HeaderRole::Request, ConnectionOccurrence::Repeated) => {
            vec![0x82, 0x87, 0xbf, 0x84, 0xbe]
        }
        (HeaderRole::Response, ConnectionOccurrence::First) => vec![
            0x88, 0x40, 0x87, 0xf2, 0xb5, 0x23, 0xa8, 0xd2, 0x95, 0x09, 0x84, 0xee, 0x3a, 0x2d,
            0x2f,
        ],
        (HeaderRole::Response, ConnectionOccurrence::Repeated) => vec![0x88, 0xbe],
        (HeaderRole::Trailers, ConnectionOccurrence::First) => vec![
            0x40, 0x87, 0xf2, 0xb5, 0x23, 0xa8, 0xd2, 0x95, 0x09, 0x84, 0xee, 0x3a, 0x2d, 0x2f,
        ],
        (HeaderRole::Trailers, ConnectionOccurrence::Repeated) => vec![0xbe],
    }
}

fn connection_block_case_v3(
    endpoint: ConnectionEndpoint,
    role: HeaderRole,
    occurrence: ConnectionOccurrence,
) -> (
    ConnectionScenarioV3,
    InitialState,
    ConnectionEvidenceV3,
    Vec<&'static str>,
) {
    let fields = connection_role_fields_v3(role);
    let outbound = matches!(
        (endpoint, role),
        (
            ConnectionEndpoint::Client,
            HeaderRole::Request | HeaderRole::Trailers
        ) | (
            ConnectionEndpoint::Server,
            HeaderRole::Response | HeaderRole::Trailers
        )
    );
    let entries = match role {
        HeaderRole::Request => 2,
        HeaderRole::Response | HeaderRole::Trailers => 1,
    };
    (
        ConnectionScenarioV3::Block {
            endpoint,
            role,
            occurrence,
            fields: fields.clone(),
        },
        InitialState::FreshDefault,
        ConnectionEvidenceV3 {
            outcomes: vec![ConnectionOutcome::Wire(connection_role_wire_v3(
                role, occurrence,
            ))],
            occurrences: fields,
            inbound_entries: if outbound { 0 } else { entries },
            outbound_entries: if outbound { entries } else { 0 },
            state: ConnectionState::Open,
            reusable: true,
            commit_sequences: if outbound {
                match occurrence {
                    ConnectionOccurrence::First => vec![1],
                    ConnectionOccurrence::Repeated => vec![1, 2],
                }
            } else {
                Vec::new()
            },
            diagnostics: None,
        },
        vec![
            "FR-001-directional-history",
            "FR-006-raw-occurrences",
            "FR-007-directional-connection",
            "FR-007-wire-order-handoff",
            "ACCEPT-CONNECTION",
        ],
    )
}

fn empty_directional_diagnostics_v3() -> DirectionalDiagnosticsEvidenceV3 {
    DirectionalDiagnosticsEvidenceV3 {
        inbound: HpackDiagnosticsEvidenceV3::default(),
        outbound: HpackDiagnosticsEvidenceV3::default(),
        inbound_effectiveness: EffectivenessEvidenceV3::Unavailable,
        outbound_effectiveness: EffectivenessEvidenceV3::Unavailable,
    }
}

fn diagnostics_case_v3(
    behavior: ConnectionBehavior,
) -> (
    ConnectionScenarioV3,
    ConnectionEvidenceV3,
    Vec<&'static str>,
) {
    let mut diagnostics = empty_directional_diagnostics_v3();
    let (blocks, outcomes, state, reusable, entries) = match behavior {
        ConnectionBehavior::DiagnosticsSuccess => {
            let first = connection_role_wire_v3(HeaderRole::Trailers, ConnectionOccurrence::First);
            let second = vec![
                0x20, 0x82, 0x00, 0x01, b'x', 0x01, 0xff, 0x1f, 0x08, 0x01, 0xff,
            ];
            let shared = HpackDiagnosticsEvidenceV3 {
                indexed_fields: 1,
                incremental_fields: 1,
                without_indexing_fields: 1,
                never_indexed_fields: 1,
                huffman_strings: 2,
                plain_strings: 3,
                table_size_updates: 1,
                table_insertions: 1,
                table_evictions: 1,
                field_octets: 41,
                wire_octets: 25,
                ..HpackDiagnosticsEvidenceV3::default()
            };
            diagnostics.inbound = HpackDiagnosticsEvidenceV3 {
                decoded_blocks: 2,
                ..shared
            };
            diagnostics.outbound = HpackDiagnosticsEvidenceV3 {
                encoded_blocks: 2,
                ..shared
            };
            diagnostics.inbound_effectiveness = EffectivenessEvidenceV3::Exact {
                encoded_wire_octets: 25,
                uncompressed_field_octets: 41,
            };
            diagnostics.outbound_effectiveness = diagnostics.inbound_effectiveness;
            (
                vec![first.clone(), second.clone()],
                vec![
                    ConnectionOutcome::Wire(first),
                    ConnectionOutcome::Wire(second),
                ],
                ConnectionState::Open,
                true,
                0,
            )
        }
        ConnectionBehavior::DiagnosticsLimit => {
            diagnostics.inbound = HpackDiagnosticsEvidenceV3 {
                decoded_blocks: 1,
                indexed_fields: 1,
                local_limit_failures: 1,
                field_octets: 10,
                wire_octets: 1,
                ..HpackDiagnosticsEvidenceV3::default()
            };
            diagnostics.inbound_effectiveness = EffectivenessEvidenceV3::Exact {
                encoded_wire_octets: 1,
                uncompressed_field_octets: 10,
            };
            (
                vec![vec![0x82]],
                vec![ConnectionOutcome::Error(ErrorCategory::HeaderListTooLarge)],
                ConnectionState::Open,
                true,
                0,
            )
        }
        ConnectionBehavior::DiagnosticsError => {
            diagnostics.inbound.compression_errors = 1;
            (
                vec![vec![0x80]],
                vec![
                    ConnectionOutcome::Error(ErrorCategory::InvalidIndex),
                    ConnectionOutcome::Error(ErrorCategory::DecoderPoisoned),
                ],
                ConnectionState::Terminal,
                false,
                0,
            )
        }
        ConnectionBehavior::DiagnosticsSaturation => {
            let saturated = HpackDiagnosticsEvidenceV3 {
                encoded_blocks: u64::MAX,
                decoded_blocks: u64::MAX,
                indexed_fields: u64::MAX,
                incremental_fields: u64::MAX,
                without_indexing_fields: u64::MAX,
                never_indexed_fields: u64::MAX,
                huffman_strings: u64::MAX,
                plain_strings: u64::MAX,
                table_size_updates: u64::MAX,
                table_insertions: u64::MAX,
                table_evictions: u64::MAX,
                compression_errors: u64::MAX,
                local_limit_failures: u64::MAX,
                field_octets: u64::MAX,
                wire_octets: u64::MAX,
            };
            diagnostics.inbound = saturated;
            diagnostics.outbound = saturated;
            (
                Vec::new(),
                vec![ConnectionOutcome::Wire(Vec::new())],
                ConnectionState::Open,
                true,
                0,
            )
        }
        _ => unreachable!("not a diagnostics behavior"),
    };
    (
        ConnectionScenarioV3::Diagnostics { behavior, blocks },
        ConnectionEvidenceV3 {
            outcomes,
            occurrences: Vec::new(),
            inbound_entries: entries,
            outbound_entries: entries,
            state,
            reusable,
            commit_sequences: if behavior == ConnectionBehavior::DiagnosticsSuccess {
                vec![1, 2]
            } else {
                Vec::new()
            },
            diagnostics: Some(diagnostics),
        },
        vec![
            "FR-009-block-byte-deltas",
            "FR-009-representation-deltas",
            "FR-009-string-deltas",
            "FR-009-table-deltas",
            "FR-009-error-limit-deltas",
            "FR-009-effectiveness",
            "FR-009-atomic-snapshot",
            "FR-009-saturation",
            "FR-009-content-free",
            "ACCEPT-DIAGNOSTICS",
        ],
    )
}

fn connection_behavior_case_v3(
    behavior: ConnectionBehavior,
) -> (
    ConnectionScenarioV3,
    InitialState,
    ConnectionEvidenceV3,
    Vec<&'static str>,
) {
    let field = Field::new(b"x-manifest", b"value", false);
    let open =
        |outcomes, inbound_entries, outbound_entries, commit_sequences| ConnectionEvidenceV3 {
            outcomes,
            occurrences: Vec::new(),
            inbound_entries,
            outbound_entries,
            state: ConnectionState::Open,
            reusable: true,
            commit_sequences,
            diagnostics: None,
        };
    match behavior {
        ConnectionBehavior::SettingsDecrease => (
            ConnectionScenarioV3::Settings {
                behavior,
                initial_capacity: 4096,
                advertised_capacities: vec![0],
                fields: Vec::new(),
            },
            InitialState::FreshDefault,
            open(vec![ConnectionOutcome::Wire(vec![0x20])], 0, 0, vec![1]),
            vec![
                "FR-007-directional-connection",
                "FR-007-wire-order-handoff",
                "ACCEPT-CONNECTION",
            ],
        ),
        ConnectionBehavior::SettingsIncrease => (
            ConnectionScenarioV3::Settings {
                behavior,
                initial_capacity: 4096,
                advertised_capacities: vec![0, 128],
                fields: Vec::new(),
            },
            InitialState::FreshDefault,
            open(
                vec![
                    ConnectionOutcome::Wire(vec![0x20]),
                    ConnectionOutcome::Wire(vec![0x3f, 0x61]),
                ],
                0,
                0,
                vec![1, 2],
            ),
            vec![
                "FR-007-directional-connection",
                "FR-007-wire-order-handoff",
                "ACCEPT-CONNECTION",
            ],
        ),
        ConnectionBehavior::LocalValidation => (
            ConnectionScenarioV3::LocalValidation {
                endpoint: ConnectionEndpoint::Server,
                role: HeaderRole::Request,
                fields: vec![Field::new(b"Upper", b"value", false)],
                decoded_limit: usize::MAX,
            },
            InitialState::FreshDefault,
            open(
                vec![ConnectionOutcome::Error(ErrorCategory::Protocol)],
                2,
                0,
                Vec::new(),
            ),
            vec![
                "FR-008-validation-precedence",
                "FR-007-directional-connection",
                "ACCEPT-CONNECTION",
            ],
        ),
        ConnectionBehavior::Cancellation => {
            let mut committed = vec![0x3f, 0x61];
            committed.extend(connection_role_wire_v3(
                HeaderRole::Trailers,
                ConnectionOccurrence::First,
            ));
            (
                ConnectionScenarioV3::Cancellation {
                    endpoint: ConnectionEndpoint::Client,
                    pending_table_size: 128,
                    fields: vec![field],
                },
                InitialState::FreshDefault,
                open(
                    vec![
                        ConnectionOutcome::Incomplete,
                        ConnectionOutcome::Wire(committed),
                    ],
                    0,
                    1,
                    vec![1],
                ),
                vec![
                    "FR-008-outbound-cancellation",
                    "FR-007-wire-order-handoff",
                    "ACCEPT-CONNECTION",
                ],
            )
        }
        ConnectionBehavior::IncompleteInput => (
            ConnectionScenarioV3::IncompleteInput {
                endpoint: ConnectionEndpoint::Server,
                stream_id: 1,
                first_fragment: vec![0x82],
            },
            InitialState::FreshDefault,
            open(vec![ConnectionOutcome::Incomplete], 0, 0, Vec::new()),
            vec!["FR-007-directional-connection", "ACCEPT-CONNECTION"],
        ),
        ConnectionBehavior::TerminalFailure => (
            ConnectionScenarioV3::TerminalFailure {
                endpoint: ConnectionEndpoint::Server,
                first_block: vec![0x80],
                repeat_block: vec![0x82],
            },
            InitialState::FreshDefault,
            ConnectionEvidenceV3 {
                outcomes: vec![
                    ConnectionOutcome::Error(ErrorCategory::InvalidIndex),
                    ConnectionOutcome::Error(ErrorCategory::DecoderPoisoned),
                ],
                occurrences: Vec::new(),
                inbound_entries: 0,
                outbound_entries: 0,
                state: ConnectionState::Terminal,
                reusable: false,
                commit_sequences: Vec::new(),
                diagnostics: None,
            },
            vec![
                "FR-008-compression-poison",
                "FR-007-directional-connection",
                "ACCEPT-CONNECTION",
            ],
        ),
        ConnectionBehavior::EncodedLimitZero
        | ConnectionBehavior::EncodedLimitBelow
        | ConnectionBehavior::EncodedLimitEqual
        | ConnectionBehavior::EncodedLimitOneOver => {
            let (limit, block, accepted) = match behavior {
                ConnectionBehavior::EncodedLimitZero => (0, Vec::new(), true),
                ConnectionBehavior::EncodedLimitBelow => (2, vec![0x82], true),
                ConnectionBehavior::EncodedLimitEqual => (1, vec![0x82], true),
                ConnectionBehavior::EncodedLimitOneOver => (0, vec![0x82], false),
                _ => unreachable!(),
            };
            (
                ConnectionScenarioV3::EncodedBoundary {
                    endpoint: ConnectionEndpoint::Server,
                    view: ConnectionView::Discard,
                    limit,
                    block,
                },
                InitialState::FreshDefault,
                if accepted {
                    open(vec![ConnectionOutcome::Wire(Vec::new())], 0, 0, Vec::new())
                } else {
                    ConnectionEvidenceV3 {
                        outcomes: vec![
                            ConnectionOutcome::Error(ErrorCategory::EncodedHeaderBlockTooLarge),
                            ConnectionOutcome::Error(ErrorCategory::EncodedHeaderBlockTooLarge),
                        ],
                        occurrences: Vec::new(),
                        inbound_entries: 0,
                        outbound_entries: 0,
                        state: ConnectionState::Terminal,
                        reusable: false,
                        commit_sequences: Vec::new(),
                        diagnostics: None,
                    }
                },
                vec![
                    "FR-008-encoded-limit",
                    "FR-008-encoded-terminal-repeat",
                    "ACCEPT-LIMIT-CONNECTION",
                ],
            )
        }
        ConnectionBehavior::DecodedLimitZero
        | ConnectionBehavior::DecodedLimitBelow
        | ConnectionBehavior::DecodedLimitEqual
        | ConnectionBehavior::DecodedLimitOneOver => {
            let (limit, accepted) = match behavior {
                ConnectionBehavior::DecodedLimitZero => (0, false),
                ConnectionBehavior::DecodedLimitBelow => (43, true),
                ConnectionBehavior::DecodedLimitEqual => (42, true),
                ConnectionBehavior::DecodedLimitOneOver => (41, false),
                _ => unreachable!(),
            };
            (
                ConnectionScenarioV3::DecodedBoundary {
                    endpoint: ConnectionEndpoint::Server,
                    limit,
                    block: vec![0x82],
                },
                InitialState::FreshDefault,
                open(
                    vec![if accepted {
                        ConnectionOutcome::Wire(Vec::new())
                    } else {
                        ConnectionOutcome::Error(ErrorCategory::HeaderListTooLarge)
                    }],
                    0,
                    0,
                    Vec::new(),
                ),
                vec!["FR-008-local-limit-sync", "ACCEPT-LIMIT-CONNECTION"],
            )
        }
        ConnectionBehavior::AllocationInbound | ConnectionBehavior::ResourceTerminalRepeat => {
            let repeat = behavior == ConnectionBehavior::ResourceTerminalRepeat;
            (
                ConnectionScenarioV3::InboundAllocation {
                    endpoint: ConnectionEndpoint::Server,
                    block: vec![0x82],
                    repeat,
                },
                InitialState::FreshDefault,
                ConnectionEvidenceV3 {
                    outcomes: if repeat {
                        vec![
                            ConnectionOutcome::Error(ErrorCategory::AllocationFailed),
                            ConnectionOutcome::Error(ErrorCategory::AllocationFailed),
                        ]
                    } else {
                        vec![ConnectionOutcome::Error(ErrorCategory::AllocationFailed)]
                    },
                    occurrences: Vec::new(),
                    inbound_entries: 0,
                    outbound_entries: 0,
                    state: ConnectionState::Terminal,
                    reusable: false,
                    commit_sequences: Vec::new(),
                    diagnostics: None,
                },
                vec![
                    "FR-008-decoder-allocation",
                    "FR-007-directional-connection",
                    "ACCEPT-LIMIT-CONNECTION",
                ],
            )
        }
        ConnectionBehavior::AllocationOutboundPreHandoff => (
            ConnectionScenarioV3::OutboundAllocation {
                endpoint: ConnectionEndpoint::Client,
                pending_table_size: 128,
                fields: vec![field],
            },
            InitialState::FreshDefault,
            {
                let mut retry_wire = vec![0x3f, 0x61];
                retry_wire.extend(connection_role_wire_v3(
                    HeaderRole::Trailers,
                    ConnectionOccurrence::First,
                ));
                open(
                    vec![
                        ConnectionOutcome::Wire(Vec::new()),
                        ConnectionOutcome::Error(ErrorCategory::AllocationFailed),
                        ConnectionOutcome::Wire(retry_wire),
                    ],
                    0,
                    1,
                    vec![1, 2],
                )
            },
            vec![
                "FR-008-outbound-allocation",
                "FR-007-wire-order-handoff",
                "ACCEPT-LIMIT-CONNECTION",
            ],
        ),
        ConnectionBehavior::DiagnosticsSuccess
        | ConnectionBehavior::DiagnosticsLimit
        | ConnectionBehavior::DiagnosticsError
        | ConnectionBehavior::DiagnosticsSaturation => {
            let (scenario, evidence, links) = diagnostics_case_v3(behavior);
            (scenario, InitialState::FreshDefault, evidence, links)
        }
    }
}

fn add_phase_two_v3_cases(cases: &mut Vec<CaseDefinition>) {
    let request = connection_role_wire_v3(HeaderRole::Request, ConnectionOccurrence::First);
    let response = connection_role_wire_v3(HeaderRole::Response, ConnectionOccurrence::First);
    for (endpoint, block, name) in [
        (ConnectionEndpoint::Server, request, "SERVER"),
        (ConnectionEndpoint::Client, response, "CLIENT"),
    ] {
        cases.push(CaseDefinition {
            id: format!("CONNECTION-{name}-COMPLETE-ENCODED-ONE-OVER-v3"),
            class: AcceptanceClass::Connection,
            input: CaseInput::ConnectionV3(ConnectionScenarioV3::EncodedBoundary {
                endpoint,
                view: ConnectionView::CompleteBlock,
                limit: block.len() - 1,
                block,
            }),
            initial_state: InitialState::FreshDefault,
            expected: ExpectedEvidence::ConnectionV3(Box::new(ConnectionEvidenceV3 {
                outcomes: vec![
                    ConnectionOutcome::Error(ErrorCategory::EncodedHeaderBlockTooLarge),
                    ConnectionOutcome::Error(ErrorCategory::EncodedHeaderBlockTooLarge),
                ],
                occurrences: Vec::new(),
                inbound_entries: 0,
                outbound_entries: 0,
                state: ConnectionState::Terminal,
                reusable: false,
                commit_sequences: Vec::new(),
                diagnostics: None,
            })),
            requirement_links: vec![
                "FR-008-encoded-limit",
                "FR-008-encoded-terminal-repeat",
                "ACCEPT-LIMIT-CONNECTION",
            ],
        });
    }
    for (endpoint, name) in [
        (ConnectionEndpoint::Server, "SERVER"),
        (ConnectionEndpoint::Client, "CLIENT"),
    ] {
        cases.push(CaseDefinition {
            id: format!("CONNECTION-{name}-ENCODED-ACCOUNTING-OVERFLOW-v3"),
            class: AcceptanceClass::Connection,
            input: CaseInput::ConnectionV3(ConnectionScenarioV3::EncodedAccountingOverflow {
                endpoint,
                stream_id: 1,
                first_fragment: vec![0x82],
                accounted_len: usize::MAX,
                continuation: vec![0],
            }),
            initial_state: InitialState::FreshDefault,
            expected: ExpectedEvidence::ConnectionV3(Box::new(ConnectionEvidenceV3 {
                outcomes: vec![
                    ConnectionOutcome::Incomplete,
                    ConnectionOutcome::Error(ErrorCategory::EncodedHeaderBlockTooLarge),
                    ConnectionOutcome::Error(ErrorCategory::EncodedHeaderBlockTooLarge),
                ],
                occurrences: Vec::new(),
                inbound_entries: 0,
                outbound_entries: 0,
                state: ConnectionState::Terminal,
                reusable: false,
                commit_sequences: Vec::new(),
                diagnostics: None,
            })),
            requirement_links: vec![
                "FR-008-encoded-accounting-overflow",
                "FR-008-encoded-terminal-repeat",
                "ACCEPT-LIMIT-CONNECTION",
            ],
        });
        cases.push(CaseDefinition {
            id: format!("CONNECTION-{name}-ASSEMBLY-ALLOCATION-v3"),
            class: AcceptanceClass::Connection,
            input: CaseInput::ConnectionV3(ConnectionScenarioV3::AssemblyAllocation {
                endpoint,
                stream_id: 1,
                first_fragment: vec![0x82],
                continuation: vec![0; 32],
            }),
            initial_state: InitialState::FreshDefault,
            expected: ExpectedEvidence::ConnectionV3(Box::new(ConnectionEvidenceV3 {
                outcomes: vec![
                    ConnectionOutcome::Incomplete,
                    ConnectionOutcome::Error(ErrorCategory::AllocationFailed),
                    ConnectionOutcome::Error(ErrorCategory::AllocationFailed),
                ],
                occurrences: Vec::new(),
                inbound_entries: 0,
                outbound_entries: 0,
                state: ConnectionState::Terminal,
                reusable: false,
                commit_sequences: Vec::new(),
                diagnostics: None,
            })),
            requirement_links: vec![
                "FR-008-assembly-allocation",
                "FR-007-directional-connection",
                "ACCEPT-LIMIT-CONNECTION",
            ],
        });
    }
}

pub fn manifest_v3() -> Vec<CaseDefinition> {
    let mut cases = manifest_v2()
        .into_iter()
        .map(|mut case| {
            case.id = format!(
                "{}-v3",
                case.id
                    .strip_suffix("-v2")
                    .expect("Manifest v2 case ID must end in -v2")
            );
            match case.input.clone() {
                CaseInput::ConnectionBlock {
                    endpoint,
                    role,
                    occurrence,
                } => {
                    let (scenario, initial_state, evidence, links) =
                        connection_block_case_v3(endpoint, role, occurrence);
                    case.input = CaseInput::ConnectionV3(scenario);
                    case.initial_state = initial_state;
                    case.expected = ExpectedEvidence::ConnectionV3(Box::new(evidence));
                    case.requirement_links = links;
                }
                CaseInput::ConnectionBehavior(behavior) => {
                    let (scenario, initial_state, evidence, links) =
                        connection_behavior_case_v3(behavior);
                    case.input = CaseInput::ConnectionV3(scenario);
                    case.initial_state = initial_state;
                    case.expected = ExpectedEvidence::ConnectionV3(Box::new(evidence));
                    case.requirement_links = links;
                }
                _ => {}
            }
            case
        })
        .collect::<Vec<_>>();
    add_phase_two_v3_cases(&mut cases);
    cases
}

struct FreshPathEncoding {
    wire: Vec<u8>,
    table_entries: usize,
    diagnostics: HpackDiagnosticsEvidenceV3,
}

fn path_static_exact(field: &Field) -> Option<usize> {
    match (field.name.as_slice(), field.value.as_slice()) {
        (b":method", b"GET") => Some(2),
        (b":method", b"POST") => Some(3),
        (b":path", b"/") => Some(4),
        (b":scheme", b"http") => Some(6),
        (b":scheme", b"https") => Some(7),
        (b":status", b"200") => Some(8),
        _ => None,
    }
}

fn path_static_name(name: &[u8]) -> Option<usize> {
    match name {
        b":method" => Some(2),
        b":path" => Some(4),
        b":scheme" => Some(6),
        b":status" => Some(8),
        b"authorization" => Some(23),
        b"content-type" => Some(31),
        _ => None,
    }
}

fn push_path_string(
    output: &mut Vec<u8>,
    value: &[u8],
    diagnostics: &mut HpackDiagnosticsEvidenceV3,
) {
    let huffman = reference_huffman::encode(value);
    if huffman.len() < value.len() {
        push_integer(output, huffman.len(), 7, 0x80);
        output.extend_from_slice(&huffman);
        diagnostics.huffman_strings += 1;
    } else {
        push_integer(output, value.len(), 7, 0);
        output.extend_from_slice(value);
        diagnostics.plain_strings += 1;
    }
}

fn fresh_path_encoding(fields: &[Field]) -> FreshPathEncoding {
    let mut output = Vec::new();
    let mut dynamic = Vec::<Field>::new();
    let mut diagnostics = HpackDiagnosticsEvidenceV3 {
        encoded_blocks: 1,
        ..HpackDiagnosticsEvidenceV3::default()
    };
    for field in fields {
        diagnostics.field_octets += (field.name.len() + field.value.len()) as u64;
        let dynamic_exact = dynamic
            .iter()
            .position(|candidate| candidate.name == field.name && candidate.value == field.value);
        let dynamic_name = dynamic
            .iter()
            .position(|candidate| candidate.name == field.name);

        if field.sensitive {
            let name_index = dynamic_exact
                .map(|index| 62 + index)
                .or_else(|| path_static_exact(field))
                .or_else(|| dynamic_name.map(|index| 62 + index))
                .or_else(|| path_static_name(&field.name))
                .unwrap_or(0);
            push_integer(&mut output, name_index, 4, 0x10);
            if name_index == 0 {
                push_path_string(&mut output, &field.name, &mut diagnostics);
            }
            push_path_string(&mut output, &field.value, &mut diagnostics);
            diagnostics.never_indexed_fields += 1;
            continue;
        }

        if let Some(index) = path_static_exact(field) {
            push_integer(&mut output, index, 7, 0x80);
            diagnostics.indexed_fields += 1;
            continue;
        }
        if let Some(index) = dynamic_exact {
            push_integer(&mut output, 62 + index, 7, 0x80);
            diagnostics.indexed_fields += 1;
            continue;
        }

        let name_index = dynamic_name
            .map(|index| 62 + index)
            .or_else(|| path_static_name(&field.name))
            .unwrap_or(0);
        push_integer(&mut output, name_index, 6, 0x40);
        if name_index == 0 {
            push_path_string(&mut output, &field.name, &mut diagnostics);
        }
        push_path_string(&mut output, &field.value, &mut diagnostics);
        diagnostics.incremental_fields += 1;
        diagnostics.table_insertions += 1;
        dynamic.insert(0, field.clone());
    }
    diagnostics.wire_octets = output.len() as u64;
    FreshPathEncoding {
        wire: output,
        table_entries: dynamic.len(),
        diagnostics,
    }
}

fn path_roles_v4(path: u8, input: PathInput) -> Vec<PathRoleV4> {
    let pseudo_role = match input {
        PathInput::PseudoRequest => Some(0),
        PathInput::PseudoResponse => Some(1),
        PathInput::PseudoTrailers => Some(2),
        _ => None,
    };
    match path {
        1 => vec![PathRoleV4::Direct],
        2 => match pseudo_role {
            Some(0) => vec![PathRoleV4::ServerRequest],
            Some(1) => vec![PathRoleV4::ServerResponse],
            Some(2) => vec![PathRoleV4::ServerTrailers],
            _ => vec![
                PathRoleV4::ServerRequest,
                PathRoleV4::ServerResponse,
                PathRoleV4::ServerTrailers,
            ],
        },
        3 => match pseudo_role {
            Some(0) => vec![PathRoleV4::ClientRequest],
            Some(1) => vec![PathRoleV4::ClientResponse],
            Some(2) => vec![PathRoleV4::ClientTrailers],
            _ => vec![
                PathRoleV4::ClientRequest,
                PathRoleV4::ClientResponse,
                PathRoleV4::ClientTrailers,
            ],
        },
        4 => vec![PathRoleV4::TextProjection],
        5 => match pseudo_role {
            Some(0) => vec![PathRoleV4::StackRequest],
            Some(1) => vec![PathRoleV4::StackResponse],
            Some(2) => vec![PathRoleV4::StackTrailers],
            _ => vec![
                PathRoleV4::StackRequest,
                PathRoleV4::StackResponse,
                PathRoleV4::StackTrailers,
            ],
        },
        6 => vec![PathRoleV4::FsmGrpcMetadata, PathRoleV4::FsmGrpcStatus],
        7 => vec![PathRoleV4::StackGrpcMetadata, PathRoleV4::StackGrpcStatus],
        8 => vec![PathRoleV4::PseudoForward],
        _ => unreachable!(),
    }
}

fn path_role_fields_v4(role: PathRoleV4, source: &[Field]) -> Vec<Field> {
    match role {
        PathRoleV4::ServerRequest | PathRoleV4::ClientRequest | PathRoleV4::StackRequest => {
            if source.first().is_some_and(|field| field.name == b":method") {
                let mut fields = source.to_vec();
                fields.extend([
                    Field::new(b":scheme", b"https", false),
                    Field::new(b":path", b"/", false),
                ]);
                fields
            } else {
                let mut fields = vec![
                    Field::new(b":method", b"GET", false),
                    Field::new(b":scheme", b"https", false),
                    Field::new(b":path", b"/", false),
                ];
                fields.extend_from_slice(source);
                fields
            }
        }
        PathRoleV4::ServerResponse | PathRoleV4::ClientResponse | PathRoleV4::StackResponse => {
            if source.first().is_some_and(|field| field.name == b":status") {
                source.to_vec()
            } else {
                let mut fields = vec![Field::new(b":status", b"200", false)];
                fields.extend_from_slice(source);
                fields
            }
        }
        PathRoleV4::FsmGrpcStatus | PathRoleV4::StackGrpcStatus => {
            let mut fields = source.to_vec();
            fields.push(Field::new(b"grpc-status", b"0", false));
            fields
        }
        _ => source.to_vec(),
    }
}

fn path_role_evidence_v4(scenario: &PathScenarioV4, role: PathRoleV4) -> PathRoleEvidenceV4 {
    let rejection = match (scenario.input, role) {
        (PathInput::NonText, PathRoleV4::TextProjection) => Some(PathErrorV4::ValueNotUtf8),
        (PathInput::PseudoTrailers, PathRoleV4::ServerTrailers)
        | (PathInput::PseudoTrailers, PathRoleV4::ClientTrailers)
        | (PathInput::PseudoTrailers, PathRoleV4::StackTrailers)
        | (PathInput::PseudoTrailers, PathRoleV4::TextProjection) => {
            Some(PathErrorV4::InvalidPseudoRole)
        }
        (_, PathRoleV4::PseudoForward) => Some(PathErrorV4::PseudoNotForwardable),
        _ => None,
    };
    if let Some(error) = rejection {
        return PathRoleEvidenceV4 {
            role,
            outcome: AdapterOutcome::Reject,
            occurrences: Applicability::NotApplicable,
            wire: Applicability::NotApplicable,
            table_entries: Applicability::NotApplicable,
            diagnostics: Applicability::NotApplicable,
            error: Applicability::Applicable(error),
            decoded_binary: Applicability::NotApplicable,
            reusable: Applicability::Applicable(true),
        };
    }

    let no_codec = matches!(role, PathRoleV4::TextProjection);
    let encoding =
        (!no_codec).then(|| fresh_path_encoding(&path_role_fields_v4(role, &scenario.fields)));
    PathRoleEvidenceV4 {
        role,
        outcome: AdapterOutcome::Preserve(scenario.fields.clone()),
        occurrences: Applicability::Applicable(scenario.fields.clone()),
        wire: encoding
            .as_ref()
            .map_or(Applicability::NotApplicable, |value| {
                Applicability::Applicable(value.wire.clone())
            }),
        table_entries: encoding
            .as_ref()
            .map_or(Applicability::NotApplicable, |value| {
                Applicability::Applicable(value.table_entries)
            }),
        diagnostics: encoding
            .as_ref()
            .map_or(Applicability::NotApplicable, |value| {
                Applicability::Applicable(value.diagnostics)
            }),
        error: Applicability::NotApplicable,
        decoded_binary: if matches!(
            role,
            PathRoleV4::FsmGrpcMetadata
                | PathRoleV4::FsmGrpcStatus
                | PathRoleV4::StackGrpcMetadata
                | PathRoleV4::StackGrpcStatus
        ) {
            scenario.decoded_binary.clone()
        } else {
            Applicability::NotApplicable
        },
        reusable: Applicability::Applicable(true),
    }
}

fn add_path_v4_case(
    cases: &mut Vec<CaseDefinition>,
    path: u8,
    input: PathInput,
    name: &str,
    fields: Vec<Field>,
    decoded_binary: Applicability<Vec<u8>>,
) {
    let scenario = PathScenarioV4 {
        path,
        input,
        fields,
        roles: path_roles_v4(path, input),
        decoded_binary,
    };
    let expected = PathEvidenceV4 {
        path,
        input,
        source: scenario.fields.clone(),
        source_unchanged: true,
        roles: scenario
            .roles
            .iter()
            .copied()
            .map(|role| path_role_evidence_v4(&scenario, role))
            .collect(),
    };
    add(
        cases,
        format!("PATH-{path:02}-{name}-v4"),
        AcceptanceClass::Path,
        CaseInput::PathV4(scenario),
        InitialState::FreshDefault,
        ExpectedEvidence::PathV4(expected),
        &["FR-006-byte-preserving", "FR-010-adapter-sensitivity"],
    );
}

fn add_path_v4_cases(cases: &mut Vec<CaseDefinition>) {
    for path in 1..=7 {
        for (input, name, fields, binary) in [
            (
                PathInput::Ordinary,
                "ORDINARY",
                vec![Field::new(b"x-path", b"value", false)],
                Applicability::NotApplicable,
            ),
            (
                PathInput::Sensitive,
                "SENSITIVE",
                vec![Field::new(b"authorization", b"secret", true)],
                Applicability::NotApplicable,
            ),
            (
                PathInput::Duplicate,
                "DUPLICATE",
                vec![
                    Field::new(b"x-duplicate", b"one", false),
                    Field::new(b"x-duplicate", b"two", false),
                ],
                Applicability::NotApplicable,
            ),
            (
                PathInput::Mixed,
                "MIXED-ORDERED",
                vec![
                    Field::new(b"x-a", b"one", false),
                    Field::new(b"x-b", b"secret", true),
                    Field::new(b"x-a", b"three", true),
                ],
                Applicability::NotApplicable,
            ),
            (
                PathInput::NonText,
                "LEGAL-NON-TEXT",
                if path <= 5 {
                    vec![Field::new(b"x-bytes", [0x80, 0xff], false)]
                } else {
                    vec![Field::new(b"trace-bin", b"AID/", false)]
                },
                if path >= 6 {
                    Applicability::Applicable(vec![0, 0x80, 0xff])
                } else {
                    Applicability::NotApplicable
                },
            ),
        ] {
            add_path_v4_case(cases, path, input, name, fields, binary);
        }
    }

    for path in 2..=5 {
        for (input, name, field) in [
            (
                PathInput::PseudoRequest,
                "PSEUDO-REQUEST",
                Field::new(b":method", b"GET", false),
            ),
            (
                PathInput::PseudoResponse,
                "PSEUDO-RESPONSE",
                Field::new(b":status", b"200", false),
            ),
            (
                PathInput::PseudoTrailers,
                "PSEUDO-TRAILERS-REJECT",
                Field::new(b":status", b"200", false),
            ),
        ] {
            add_path_v4_case(
                cases,
                path,
                input,
                name,
                vec![field],
                Applicability::NotApplicable,
            );
        }
    }
    add_path_v4_case(
        cases,
        8,
        PathInput::PseudoForward,
        "PSEUDO-FORWARD",
        vec![Field::new(b":method", b"GET", false)],
        Applicability::NotApplicable,
    );
}

pub fn manifest_v4() -> Vec<CaseDefinition> {
    let mut cases = manifest_v3()
        .into_iter()
        .filter(|case| case.class != AcceptanceClass::Path)
        .map(|mut case| {
            case.id = format!(
                "{}-v4",
                case.id
                    .strip_suffix("-v3")
                    .expect("Manifest v3 case ID must end in -v3")
            );
            case
        })
        .collect::<Vec<_>>();
    add_path_v4_cases(&mut cases);
    cases
}
