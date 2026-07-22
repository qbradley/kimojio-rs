#[cfg(feature = "hpack-test-support")]
use kimojio_fsm_http::H2ErrorCode;
#[cfg(feature = "hpack-test-support")]
use kimojio_fsm_http::hpack_test_support::{
    encoder_diagnostics, encoder_table, fail_decoder_allocation_after,
    fail_encoder_allocation_after,
};
use kimojio_fsm_http::{
    H2ByteClientEvent, H2ByteClientEventRef, H2ByteStreamEvent, H2ByteStreamEventRef, H2Client,
    H2Header, H2HeaderBlockDecoder, H2HeaderBlockEncoder, H2HeaderField, H2HeaderProjectionError,
    H2HpackDiagnosticsSnapshot, H2HpackEffectiveness, H2HpackError, H2Limits, H2OutboundBlockRef,
    H2OutboundCommit, H2OutboundHeaderBlock, H2ProtocolError, H2RawHeader, H2RawHeaderRef,
    H2Server, ServerError,
};

mod support;

#[cfg(feature = "hpack-test-support")]
use support::hpack_execution_ledger::{CompletionLedger, Runner, RunnerResult};
#[cfg(feature = "hpack-test-support")]
use support::hpack_manifest::{
    ActualEvidence, ApiCase, ApiInput, Applicability, CaseInput, Disposition, ErrorCategory,
    LimitEvidence, LimitOperation,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ApiPhase {
    One,
    Two,
    Three,
}

#[derive(Clone, Copy)]
struct ApiRow {
    symbol: &'static str,
    phase: ApiPhase,
}

type DecodeWithLimit =
    fn(&mut H2HeaderBlockDecoder, &[u8], usize) -> Result<Vec<H2RawHeader>, ServerError>;
type TryDecodeWithLimit =
    fn(&mut H2HeaderBlockDecoder, &[u8], usize) -> Result<Vec<H2HeaderField>, H2ProtocolError>;
type ServerAcceptBytesRef =
    for<'a, 'b> fn(
        &'b mut H2Server,
        &'a [u8],
    ) -> Result<(Option<H2ByteStreamEventRef<'a>>, usize, Vec<u8>), ServerError>;
type ClientAcceptBytesRef =
    for<'a, 'b> fn(
        &'b mut H2Client,
        &'a [u8],
    ) -> Result<(Option<H2ByteClientEventRef<'a>>, usize, Vec<u8>), ServerError>;

#[test]
fn compatibility_decoder_rejects_sensitive_occurrences() {
    let sensitive = H2HeaderField::new(b"x-secret", b"value").with_sensitive(true);
    let retained = H2HeaderField::new(b"x-retained", b"value");
    let mut encoder = H2HeaderBlockEncoder::new();
    let wire = encoder
        .try_encode_fields(&[sensitive.clone(), retained.clone()])
        .unwrap();
    let indexed = encoder
        .try_encode_fields(std::slice::from_ref(&retained))
        .unwrap();
    assert_eq!(wire[0] & 0xf0, 0x10);

    let mut compatibility = H2HeaderBlockDecoder::new();
    assert!(matches!(
        compatibility.decode_with_limit(&wire, usize::MAX),
        Err(ServerError::InvalidHpack)
    ));
    assert_eq!(
        compatibility
            .decode_with_limit(&indexed, usize::MAX)
            .unwrap(),
        [H2RawHeader::new(b"x-retained", b"value")]
    );

    assert_eq!(
        H2HeaderBlockDecoder::new()
            .try_decode_with_limit(&wire, usize::MAX)
            .unwrap(),
        [sensitive, retained]
    );
}

#[cfg(feature = "hpack-test-support")]
#[test]
fn fallible_encoder_transaction_copy_preserves_source_on_failure() {
    let retained = H2HeaderField::new(b"x-retained", b"history");
    let mut source = H2HeaderBlockEncoder::new();
    source
        .try_encode_fields(std::slice::from_ref(&retained))
        .unwrap();
    let table_before = encoder_table(&mut source);
    let diagnostics_before = encoder_diagnostics(&mut source);

    for successful_allocations in 0..4 {
        fail_encoder_allocation_after(&mut source, Some(successful_allocations));
        assert!(matches!(
            source.try_clone_for_transaction(),
            Err(H2HpackError::AllocationFailed)
        ));
        assert_eq!(encoder_table(&mut source), table_before);
        assert_eq!(encoder_diagnostics(&mut source), diagnostics_before);
    }

    fail_encoder_allocation_after(&mut source, None);
    let mut staged = source.try_clone_for_transaction().unwrap();
    let next = H2HeaderField::new(b"x-next", b"value");
    assert_eq!(
        source
            .try_encode_fields(std::slice::from_ref(&next))
            .unwrap(),
        staged
            .try_encode_fields(std::slice::from_ref(&next))
            .unwrap()
    );
}

const API_LEDGER: &[ApiRow] = &[
    ApiRow {
        symbol: "H2RawHeader",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2RawHeader::new",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2RawHeader::as_ref",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2HeaderField",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2HeaderField::with_sensitive",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2HeaderField::as_ref",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2RawHeaderRef",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2RawHeaderRef::new",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2RawHeaderRef::with_sensitive",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2RawHeaderRef::to_owned",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2HeaderBlockEncoder",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2HeaderBlockEncoder::set_max_table_size",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2HeaderBlockEncoder::encode",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2HeaderBlockEncoder::try_encode",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2HeaderBlockEncoder::try_encode_fields",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2HeaderBlockEncoder::try_encode_ref",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2HeaderBlockDecoder",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2HeaderBlockDecoder::set_max_table_size",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2HeaderBlockDecoder::decode_with_limit",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2HeaderBlockDecoder::try_decode_with_limit",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2HpackError",
        phase: ApiPhase::One,
    },
    ApiRow {
        symbol: "H2Header",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2ByteStreamEvent",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2ByteStreamEventRef",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2ByteClientEvent",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2ByteClientEventRef",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2HeaderProjectionError",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2HpackDiagnosticsSnapshot",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2HpackEffectiveness",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::accept_event_bytes",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::accept_event_bytes_ref",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::accept_frame_bytes",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::accept_frame_bytes_ref",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::accept_frame_bytes_typed",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::accept_frame_bytes_ref_typed",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::accept_bytes",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::accept_bytes_ref",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::accept_frame_bytes",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::accept_frame_bytes_ref",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::accept_frame_bytes_typed",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::accept_frame_bytes_ref_typed",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::response_frames",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::response_frames_with_headers",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::response_headers_frame",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::trailers_frame",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::open_stream",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::trailers_frame",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::response_frames_with_raw_headers",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::response_headers_frame_with_raw_headers",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::trailers_frame_with_raw_headers",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::open_stream_with_raw_headers",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::trailers_frame_with_raw_headers",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Limits::max_encoded_header_block_size",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::inbound_hpack_diagnostics",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::outbound_hpack_diagnostics",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::inbound_hpack_diagnostics",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::outbound_hpack_diagnostics",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2OutboundCommit",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2OutboundBlockRef",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2OutboundHeaderBlock",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::next_outbound_block",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::acknowledge_outbound_block",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::prepare_outbound_header_block",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::next_outbound_block",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::acknowledge_outbound_block",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::prepare_outbound_header_block",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::accept_complete_header_block_bytes",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Server::accept_complete_header_block",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::accept_complete_header_block_bytes",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "H2Client::accept_complete_header_block",
        phase: ApiPhase::Two,
    },
    ApiRow {
        symbol: "stack_http::HeaderField::sensitive",
        phase: ApiPhase::Three,
    },
    ApiRow {
        symbol: "fsm_grpc::Metadata::as_h2_fields",
        phase: ApiPhase::Three,
    },
    ApiRow {
        symbol: "stack_grpc::Metadata::as_h2_fields",
        phase: ApiPhase::Three,
    },
];

#[test]
fn hpack_api_codec_symbols() {
    const _: H2HeaderBlockEncoder = H2HeaderBlockEncoder::new();
    let _: fn(&mut H2HeaderBlockEncoder, &[H2RawHeader]) -> Vec<u8> = H2HeaderBlockEncoder::encode;
    let _: fn(&mut H2HeaderBlockEncoder, &[H2RawHeader]) -> Result<Vec<u8>, H2HpackError> =
        H2HeaderBlockEncoder::try_encode;
    let _: fn(&mut H2HeaderBlockEncoder, &[H2HeaderField]) -> Result<Vec<u8>, H2HpackError> =
        H2HeaderBlockEncoder::try_encode_fields;
    let _: fn(&mut H2HeaderBlockEncoder, &[H2RawHeaderRef<'_>]) -> Result<Vec<u8>, H2HpackError> =
        H2HeaderBlockEncoder::try_encode_ref;
    let _: DecodeWithLimit = H2HeaderBlockDecoder::decode_with_limit;
    let _: TryDecodeWithLimit = H2HeaderBlockDecoder::try_decode_with_limit;
    let pair = H2RawHeader {
        name: b"x-source-compatible".to_vec(),
        value: b"value".to_vec(),
    };
    assert_eq!(pair.name, b"x-source-compatible");
    let errors = [
        H2HpackError::HeaderIndexOutOfBounds,
        H2HpackError::IntegerDecoding,
        H2HpackError::StringDecoding,
        H2HpackError::InvalidMaxDynamicSize,
        H2HpackError::InvalidTableSizeUpdate,
        H2HpackError::InvalidHuffman,
        H2HpackError::TableSizeUpdateAfterField,
        H2HpackError::HeaderListTooLarge,
        H2HpackError::EncodedHeaderBlockTooLarge,
        H2HpackError::FieldSizeOverflow,
        H2HpackError::StateOverflow,
        H2HpackError::DecoderPoisoned,
        H2HpackError::AllocationFailed,
    ];
    assert!(errors.iter().all(|error| !error.to_string().is_empty()));

    let mut symbols = std::collections::BTreeSet::new();
    for row in API_LEDGER {
        assert!(
            symbols.insert(row.symbol),
            "duplicate API row {}",
            row.symbol
        );
    }

    assert_eq!(
        API_LEDGER
            .iter()
            .filter(|row| row.phase == ApiPhase::One)
            .count(),
        21
    );
}

#[test]
fn hpack_api_phase_two_symbols() {
    let _: fn(&mut H2Server, &[u8]) -> Result<_, ServerError> = H2Server::accept_event_bytes;
    let _: ServerAcceptBytesRef = H2Server::accept_event_bytes_ref;
    let _: fn(&mut H2Client, &[u8]) -> Result<_, ServerError> = H2Client::accept_bytes;
    let _: ClientAcceptBytesRef = H2Client::accept_bytes_ref;
    let _: fn(&H2Server) -> H2HpackDiagnosticsSnapshot = H2Server::inbound_hpack_diagnostics;
    let _: fn(&H2Server) -> H2HpackDiagnosticsSnapshot = H2Server::outbound_hpack_diagnostics;
    let _: fn(&H2Client) -> H2HpackDiagnosticsSnapshot = H2Client::inbound_hpack_diagnostics;
    let _: fn(&H2Client) -> H2HpackDiagnosticsSnapshot = H2Client::outbound_hpack_diagnostics;

    let _: Option<H2ByteStreamEvent> = None;
    let _: Option<H2ByteClientEvent> = None;
    let _: Option<H2HeaderProjectionError> = None;
    let _: Option<H2HpackEffectiveness> = None;
    let _: Option<H2OutboundCommit> = None;
    let _: Option<H2OutboundBlockRef<'_>> = None;
    let _: Option<H2OutboundHeaderBlock<'_, '_>> = None;
    let _: fn(&mut H2Server, u32, u8, &[u8]) -> Result<H2ByteStreamEvent, ServerError> =
        H2Server::accept_complete_header_block_bytes;
    let _: fn(&mut H2Client, u32, u8, &[u8]) -> Result<H2ByteClientEvent, ServerError> =
        H2Client::accept_complete_header_block_bytes;
    let projected = H2ByteStreamEvent::<Vec<u8>>::Trailers {
        stream_id: 1,
        headers: vec![H2HeaderField::new(b"x", b"\xff")],
    }
    .try_into_text();
    assert_eq!(projected, Err(H2HeaderProjectionError::ValueNotUtf8));
    assert!(H2Header::new("x", "value").with_sensitive(true).sensitive);
    let limits = H2Limits {
        max_encoded_header_block_size: 1,
        max_header_list_size: 2,
        ..H2Limits::default()
    };
    assert_ne!(
        limits.max_encoded_header_block_size,
        limits.max_header_list_size
    );
    assert_eq!(
        API_LEDGER
            .iter()
            .filter(|row| row.phase == ApiPhase::Two)
            .count(),
        49
    );
}

#[cfg(feature = "hpack-test-support")]
fn api_case_id(symbol: &str) -> String {
    format!(
        "API-{}-v4",
        symbol
            .replace("::", "-")
            .replace('_', "-")
            .to_ascii_uppercase()
    )
}

#[cfg(feature = "hpack-test-support")]
fn execute_api(input: ApiInput) -> ActualEvidence {
    let pair = || H2RawHeader::new(b"x", b"value");
    let field = || H2HeaderField::new(b"x", b"value");
    match input.case {
        ApiCase::RawHeaderType => {
            let value: H2RawHeader = pair();
            assert_eq!(value.name, b"x");
        }
        ApiCase::RawHeaderNew => assert_eq!(pair().value, b"value"),
        ApiCase::RawHeaderAsRef => assert_eq!(pair().as_ref().name, b"x"),
        ApiCase::HeaderFieldType => {
            let value: H2HeaderField = field();
            assert!(!value.sensitive);
        }
        ApiCase::HeaderFieldSensitive => assert!(field().with_sensitive(true).sensitive),
        ApiCase::HeaderFieldAsRef => assert_eq!(field().as_ref().value, b"value"),
        ApiCase::RawHeaderRefType => {
            let value: H2RawHeaderRef<'_> = H2RawHeaderRef::new(b"x", b"value");
            assert!(!value.sensitive);
        }
        ApiCase::RawHeaderRefNew => assert_eq!(H2RawHeaderRef::new(b"x", b"value").name, b"x"),
        ApiCase::RawHeaderRefSensitive => {
            assert!(
                H2RawHeaderRef::new(b"x", b"value")
                    .with_sensitive(true)
                    .sensitive
            );
        }
        ApiCase::RawHeaderRefToOwned => {
            assert_eq!(H2RawHeaderRef::new(b"x", b"value").to_owned(), field());
        }
        ApiCase::EncoderType => {
            let _: H2HeaderBlockEncoder = H2HeaderBlockEncoder::new();
        }
        ApiCase::EncoderSetCapacity => {
            let mut encoder = H2HeaderBlockEncoder::new();
            encoder.set_max_table_size(0);
            assert_eq!(encoder.try_encode(&[]).unwrap(), [0x20]);
        }
        ApiCase::EncoderEncode => {
            assert!(!H2HeaderBlockEncoder::new().encode(&[pair()]).is_empty());
        }
        ApiCase::EncoderTryEncode => {
            assert!(H2HeaderBlockEncoder::new().try_encode(&[pair()]).is_ok());
        }
        ApiCase::EncoderTryEncodeFields => {
            assert!(
                H2HeaderBlockEncoder::new()
                    .try_encode_fields(&[field()])
                    .is_ok()
            );
        }
        ApiCase::EncoderTryEncodeRef => {
            assert!(
                H2HeaderBlockEncoder::new()
                    .try_encode_ref(&[H2RawHeaderRef::new(b"x", b"value")])
                    .is_ok()
            );
        }
        ApiCase::DecoderType => {
            let _: H2HeaderBlockDecoder = H2HeaderBlockDecoder::new();
        }
        ApiCase::DecoderSetCapacity => {
            let mut decoder = H2HeaderBlockDecoder::new();
            decoder.set_max_table_size(0);
            assert!(
                decoder
                    .try_decode_with_limit(&[0x20], usize::MAX)
                    .unwrap()
                    .is_empty()
            );
        }
        ApiCase::DecoderDecodeWithLimit => {
            assert_eq!(
                H2HeaderBlockDecoder::new()
                    .decode_with_limit(&[0x82], usize::MAX)
                    .unwrap(),
                [H2RawHeader::new(b":method", b"GET")]
            );
        }
        ApiCase::DecoderTryDecodeWithLimit => {
            assert_eq!(
                H2HeaderBlockDecoder::new()
                    .try_decode_with_limit(&[0x82], usize::MAX)
                    .unwrap(),
                [H2HeaderField::new(b":method", b"GET")]
            );
        }
        ApiCase::HpackErrorType => {
            let error: H2HpackError = H2HpackError::IntegerDecoding;
            assert!(!error.to_string().is_empty());
        }
    }
    ActualEvidence::Api {
        case: input.case,
        symbol: input.symbol,
    }
}

#[cfg(feature = "hpack-test-support")]
#[test]
fn phase_one_public_api_and_allocation_cases_complete_exactly_once() {
    let mut completion = CompletionLedger::phase_one(Runner::HpackApi);
    let cases = completion.cases().cloned().collect::<Vec<_>>();
    for case in cases {
        assert_eq!(
            case.initial_state,
            support::hpack_manifest::InitialState::FreshDefault
        );
        let actual = match &case.input {
            CaseInput::Api(input) => {
                assert!(
                    API_LEDGER
                        .iter()
                        .any(|row| row.phase == ApiPhase::One && row.symbol == input.symbol)
                );
                assert_eq!(case.id, api_case_id(input.symbol));
                execute_api(*input)
            }
            CaseInput::Limit(input)
                if matches!(&input.operation, LimitOperation::AllocationOutbound { .. }) =>
            {
                let LimitOperation::AllocationOutbound {
                    field: manifest_field,
                    capacity_updates,
                } = &input.operation
                else {
                    unreachable!()
                };
                let field = H2RawHeader::new(
                    manifest_field.name.as_slice(),
                    manifest_field.value.as_slice(),
                );
                let mut encoder = H2HeaderBlockEncoder::new();
                for capacity in capacity_updates {
                    encoder.set_max_table_size(*capacity);
                }
                fail_encoder_allocation_after(&mut encoder, Some(0));
                assert_eq!(
                    encoder.try_encode(std::slice::from_ref(&field)),
                    Err(H2HpackError::AllocationFailed)
                );
                fail_encoder_allocation_after(&mut encoder, None);
                let retried = encoder.try_encode(std::slice::from_ref(&field)).unwrap();
                let mut baseline = H2HeaderBlockEncoder::new();
                for capacity in capacity_updates {
                    baseline.set_max_table_size(*capacity);
                }
                assert_eq!(
                    retried,
                    baseline.try_encode(std::slice::from_ref(&field)).unwrap()
                );
                ActualEvidence::Limit(LimitEvidence {
                    outcome: Err(ErrorCategory::AllocationFailed),
                    disposition: Disposition::Reusable,
                    table_entries: Vec::new(),
                    header_list_too_large_delta: 0,
                    compression_error_delta: 0,
                    accounting: None,
                    post_limit_allocations: Applicability::NotApplicable,
                })
            }
            CaseInput::Limit(input)
                if matches!(&input.operation, LimitOperation::AllocationInbound { .. }) =>
            {
                let LimitOperation::AllocationInbound {
                    field: manifest_field,
                } = &input.operation
                else {
                    unreachable!()
                };
                let field = H2RawHeader::new(
                    manifest_field.name.as_slice(),
                    manifest_field.value.as_slice(),
                );
                let block = H2HeaderBlockEncoder::new()
                    .try_encode(std::slice::from_ref(&field))
                    .unwrap();
                let mut decoder = H2HeaderBlockDecoder::new();
                fail_decoder_allocation_after(&mut decoder, Some(0));
                let first = decoder
                    .try_decode_with_limit(&block, usize::MAX)
                    .unwrap_err();
                assert_eq!(first.code, H2ErrorCode::InternalError);
                assert_eq!(first.hpack_error, Some(H2HpackError::AllocationFailed));
                fail_decoder_allocation_after(&mut decoder, None);
                assert_eq!(
                    decoder.try_decode_with_limit(&[0x82], usize::MAX),
                    Err(first)
                );
                ActualEvidence::Limit(LimitEvidence {
                    outcome: Err(ErrorCategory::AllocationFailed),
                    disposition: Disposition::AllocationTerminal,
                    table_entries: Vec::new(),
                    header_list_too_large_delta: 0,
                    compression_error_delta: 0,
                    accounting: None,
                    post_limit_allocations: Applicability::NotApplicable,
                })
            }
            _ => unreachable!("unexpected API runner case {}", case.id),
        };
        completion.complete(RunnerResult {
            id: case.id,
            input: case.input,
            initial_state: case.initial_state,
            actual,
        });
    }
    completion.finish();
}
