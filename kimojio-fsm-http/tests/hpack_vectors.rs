#[cfg(feature = "hpack-test-support")]
use kimojio_fsm_http::hpack_test_support::{
    account_lengths, decode_integer, decoder_diagnostics, decoder_post_limit_allocations,
    decoder_table, encode_huffman, encoder_diagnostics, encoder_table,
};
use kimojio_fsm_http::{H2HeaderBlockDecoder, H2HeaderBlockEncoder};
#[cfg(feature = "hpack-test-support")]
use kimojio_fsm_http::{H2HeaderField, H2HpackError, H2RawHeaderRef};

mod support;

#[cfg(feature = "hpack-test-support")]
use support::hpack_execution_ledger::{CompletionLedger, Runner, RunnerResult};
#[cfg(feature = "hpack-test-support")]
use support::hpack_manifest::{
    AccountingEvidence, ActualEvidence, Applicability, CaseDefinition, CaseInput,
    DiagnosticEvidence, Disposition, ErrorCategory, Field, HuffmanEvidence, HuffmanKind,
    InitialState, IntegerCodecContext, IntegerEvidence, LimitEvidence, LimitOperation,
    PostLimitAllocationEvidence, RepresentationCase, RepresentationEvidence, TableAction,
    TableActualEvidence, TableEntries, TablePopulation, TableStateOwner,
};

#[cfg(feature = "hpack-test-support")]
fn field(name: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>, sensitive: bool) -> H2HeaderField {
    H2HeaderField::new(name, value).with_sensitive(sensitive)
}

#[cfg(feature = "hpack-test-support")]
fn observed_field(field: &H2HeaderField) -> Field {
    Field {
        name: field.name.clone(),
        value: field.value.clone(),
        sensitive: field.sensitive,
    }
}

#[cfg(feature = "hpack-test-support")]
fn observed_table_entries(entries: Vec<(Vec<u8>, Vec<u8>)>) -> TableEntries {
    let fields = entries
        .into_iter()
        .map(|(name, value)| Field {
            name,
            value,
            sensitive: false,
        })
        .collect::<Vec<_>>();
    if let Some(first) = fields.first()
        && fields.len() > 1
        && fields.iter().all(|field| field == first)
    {
        return TableEntries::Repeated {
            field: first.clone(),
            count: fields.len(),
        };
    }
    TableEntries::Exact(fields)
}

#[cfg(feature = "hpack-test-support")]
fn category(error: H2HpackError) -> ErrorCategory {
    match error {
        H2HpackError::HeaderIndexOutOfBounds => ErrorCategory::InvalidIndex,
        H2HpackError::IntegerDecoding => ErrorCategory::Integer,
        H2HpackError::StringDecoding => ErrorCategory::String,
        H2HpackError::InvalidMaxDynamicSize => ErrorCategory::InvalidMaximum,
        H2HpackError::InvalidTableSizeUpdate => ErrorCategory::InvalidTableUpdate,
        H2HpackError::InvalidHuffman => ErrorCategory::InvalidHuffman,
        H2HpackError::TableSizeUpdateAfterField => ErrorCategory::TableUpdateAfterField,
        H2HpackError::HeaderListTooLarge => ErrorCategory::HeaderListTooLarge,
        H2HpackError::FieldSizeOverflow => ErrorCategory::FieldSizeOverflow,
        H2HpackError::StateOverflow => ErrorCategory::StateOverflow,
        H2HpackError::DecoderPoisoned => ErrorCategory::DecoderPoisoned,
        H2HpackError::AllocationFailed => ErrorCategory::AllocationFailed,
        _ => unreachable!("new HPACK category requires an acceptance mapping"),
    }
}

#[cfg(feature = "hpack-test-support")]
fn protocol_category(error: kimojio_fsm_http::H2ProtocolError) -> ErrorCategory {
    category(
        error
            .hpack_error
            .expect("HPACK test errors carry a category"),
    )
}

#[cfg(feature = "hpack-test-support")]
fn observed_post_limit_allocations(
    decoder: &H2HeaderBlockDecoder,
) -> Applicability<PostLimitAllocationEvidence> {
    decoder_post_limit_allocations(decoder).map_or(Applicability::NotApplicable, |observed| {
        Applicability::Applicable(PostLimitAllocationEvidence {
            discarded_output_allocations: observed.discarded_output_allocations,
            dynamic_table_synchronization_allocations: observed
                .dynamic_table_synchronization_allocations,
        })
    })
}

#[cfg(feature = "hpack-test-support")]
fn execute_integer(case: &CaseDefinition) -> ActualEvidence {
    assert_eq!(case.initial_state, InitialState::FreshDefault);
    let CaseInput::Integer(input) = &case.input else {
        unreachable!()
    };
    let (result, consumed) = decode_integer(&input.bytes, input.prefix_bits, input.width.into());
    let mut wire = input.bytes.clone();
    wire[0] |= match input.context {
        IntegerCodecContext::IndexedField => 0x80,
        IntegerCodecContext::IncrementalName => 0x40,
        IntegerCodecContext::SizeUpdate => 0x20,
        IntegerCodecContext::WithoutIndexingName => 0,
    };
    if matches!(
        input.context,
        IntegerCodecContext::IncrementalName | IntegerCodecContext::WithoutIndexingName
    ) {
        wire.push(0);
    }
    let mut decoder = H2HeaderBlockDecoder::new();
    let codec_outcome = decoder
        .try_decode_with_limit(&wire, usize::MAX)
        .map(|fields| fields.iter().map(observed_field).collect())
        .map_err(protocol_category);
    let codec_disposition = if codec_outcome.is_ok() {
        Disposition::Reusable
    } else {
        assert_eq!(
            decoder
                .try_decode_with_limit(&[], usize::MAX)
                .unwrap_err()
                .hpack_error,
            Some(H2HpackError::DecoderPoisoned)
        );
        Disposition::Poisoned
    };
    let codec_table_capacity = decoder_table(&decoder).max_size;
    ActualEvidence::Integer(IntegerEvidence {
        result: result.map_err(category),
        consumed,
        codec_outcome,
        codec_table_capacity,
        codec_disposition,
    })
}

#[cfg(feature = "hpack-test-support")]
fn execute_huffman(case: &CaseDefinition) -> ActualEvidence {
    assert_eq!(case.initial_state, InitialState::FreshDefault);
    let CaseInput::Huffman(input) = &case.input else {
        unreachable!()
    };
    if input.kind == HuffmanKind::RfcRequest {
        let mut decoder = H2HeaderBlockDecoder::new();
        let decoded = decoder
            .try_decode_with_limit(&input.encoded, usize::MAX)
            .unwrap();
        let mut flattened = Vec::new();
        for field in decoded {
            if !flattened.is_empty() {
                flattened.push(0);
            }
            flattened.extend_from_slice(&field.name);
            flattened.push(0);
            flattened.extend_from_slice(&field.value);
        }
        return ActualEvidence::Huffman(HuffmanEvidence {
            wire: input.encoded.clone(),
            huffman_wire: None,
            encoder_wire: None,
            decoded: Ok(flattened),
            disposition: Disposition::Reusable,
        });
    }

    let mut decoder = H2HeaderBlockDecoder::new();
    let wire = {
        let mut wire = vec![0x11];
        push_integer(&mut wire, input.declared_length, 7, 0x80);
        wire.extend_from_slice(&input.encoded);
        wire
    };
    let decoded = decoder
        .try_decode_with_limit(&wire, usize::MAX)
        .map(|fields| {
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].name, b":authority");
            assert!(fields[0].sensitive);
            fields[0].value.clone()
        })
        .map_err(protocol_category);
    let disposition = if decoded.is_ok() {
        Disposition::Reusable
    } else {
        let repeated = decoder.try_decode_with_limit(&[], usize::MAX).unwrap_err();
        assert_eq!(repeated.hpack_error, Some(H2HpackError::DecoderPoisoned));
        Disposition::Poisoned
    };

    let encoder_wire = if decoded.is_ok() {
        let mut encoder = H2HeaderBlockEncoder::new();
        Some(
            encoder
                .try_encode_ref(&[
                    H2RawHeaderRef::new(b":authority", &input.source).with_sensitive(true)
                ])
                .unwrap(),
        )
    } else {
        None
    };
    ActualEvidence::Huffman(HuffmanEvidence {
        wire,
        huffman_wire: matches!(
            input.kind,
            HuffmanKind::RoundTrip | HuffmanKind::ValidPadding(_)
        )
        .then(|| encode_huffman(&input.source)),
        encoder_wire,
        decoded,
        disposition,
    })
}

#[cfg(feature = "hpack-test-support")]
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

#[cfg(feature = "hpack-test-support")]
fn execute_representation(case: &CaseDefinition) -> ActualEvidence {
    let CaseInput::Representation(input) = &case.input else {
        unreachable!()
    };
    let mut encoder = H2HeaderBlockEncoder::new();
    let mut decoder = H2HeaderBlockDecoder::new();
    let mut wire_blocks = Vec::new();
    let mut occurrences = Vec::new();
    match case.initial_state {
        InitialState::FreshDefault => {}
        InitialState::FreshCapacity(capacity) => {
            encoder.set_max_table_size(capacity);
            decoder.set_max_table_size(capacity);
        }
        InitialState::ScenarioDefined => unreachable!("Phase 1 state must be executable"),
    }
    for block in &input.blocks {
        let fields = block
            .iter()
            .map(|manifest_field| {
                field(
                    manifest_field.name.as_slice(),
                    manifest_field.value.as_slice(),
                    manifest_field.sensitive,
                )
            })
            .collect::<Vec<_>>();
        let wire = encoder.try_encode_fields(&fields).unwrap();
        let decoded = decoder.try_decode_with_limit(&wire, usize::MAX).unwrap();
        occurrences.extend(decoded.iter().map(observed_field));
        assert_eq!(decoded, fields);
        wire_blocks.push(wire);
    }

    let encoder_state = encoder_table(&mut encoder);
    let decoder_state = decoder_table(&decoder);
    assert_eq!(encoder_state.max_size, decoder_state.max_size);
    assert_eq!(encoder_state.size, decoder_state.size);
    assert_eq!(encoder_state.entries, decoder_state.entries);
    let diagnostics = if input.case == RepresentationCase::IndexedStatic {
        let encoded = encoder_diagnostics(&mut encoder);
        let decoded = decoder_diagnostics(&decoder);
        Some(DiagnosticEvidence {
            encoded_blocks: encoded.encoded_blocks,
            decoded_blocks: decoded.decoded_blocks,
            indexed_fields: encoded.indexed_fields,
            field_bytes: encoded.field_bytes,
            wire_bytes: encoded.wire_bytes,
        })
    } else {
        None
    };
    ActualEvidence::Representation(RepresentationEvidence {
        wire_blocks,
        occurrences,
        table_entries: encoder_state
            .entries
            .into_iter()
            .map(|(name, value)| Field {
                name,
                value,
                sensitive: false,
            })
            .collect(),
        table_size: encoder_state.size,
        disposition: Disposition::Reusable,
        diagnostics,
    })
}

#[cfg(feature = "hpack-test-support")]
fn execute_table_capacity(case: &CaseDefinition) -> ActualEvidence {
    let CaseInput::TableCapacity {
        capacity,
        population,
    } = case.input
    else {
        unreachable!()
    };
    assert_eq!(case.initial_state, InitialState::FreshCapacity(capacity));
    let requested = match population {
        TablePopulation::Empty => 0,
        TablePopulation::One => 1,
        TablePopulation::MaximumMinimum => capacity / 32,
    };
    let mut wire = Vec::new();
    push_integer(&mut wire, capacity, 5, 0x20);
    for _ in 0..requested {
        wire.extend_from_slice(&[0x40, 0, 0]);
    }
    let mut decoder = H2HeaderBlockDecoder::new();
    decoder.set_max_table_size(capacity);
    let decoded = decoder.try_decode_with_limit(&wire, usize::MAX).unwrap();
    assert_eq!(decoded.len(), requested);
    assert!(
        decoded
            .iter()
            .all(|field| field.name.is_empty() && field.value.is_empty())
    );
    let state = decoder_table(&decoder);
    ActualEvidence::Table(TableActualEvidence {
        wire_blocks: vec![wire],
        error: None,
        capacity: state.max_size,
        entries: observed_table_entries(state.entries),
        accounted_size: state.size,
        container_entries: state.container_capacities[0],
        disposition: Disposition::Reusable,
    })
}

#[cfg(feature = "hpack-test-support")]
fn execute_table_transition(case: &CaseDefinition) -> ActualEvidence {
    assert_eq!(case.initial_state, InitialState::FreshDefault);
    let CaseInput::TableTransition(input) = &case.input else {
        unreachable!()
    };
    let mut encoder = H2HeaderBlockEncoder::new();
    let mut decoder = H2HeaderBlockDecoder::new();
    let mut wire_blocks = Vec::new();
    let mut error = None;

    let to_fields = |fields: &[Field]| {
        fields
            .iter()
            .map(|manifest_field| {
                field(
                    manifest_field.name.as_slice(),
                    manifest_field.value.as_slice(),
                    manifest_field.sensitive,
                )
            })
            .collect::<Vec<_>>()
    };
    for action in &input.actions {
        match action {
            TableAction::SetEncoderCapacity(capacity) => {
                encoder.set_max_table_size(*capacity);
            }
            TableAction::SetDecoderCapacity(capacity) => {
                decoder.set_max_table_size(*capacity);
            }
            TableAction::RoundTrip(fields) => {
                let fields = to_fields(fields);
                let wire = encoder.try_encode_fields(&fields).unwrap();
                let decoded = decoder.try_decode_with_limit(&wire, usize::MAX).unwrap();
                assert_eq!(decoded, fields);
                wire_blocks.push(wire);
            }
            TableAction::RoundTripRepeated {
                field: repeated,
                count,
            } => {
                let fields = vec![
                    field(
                        repeated.name.as_slice(),
                        repeated.value.as_slice(),
                        repeated.sensitive,
                    );
                    *count
                ];
                let wire = encoder.try_encode_fields(&fields).unwrap();
                let decoded = decoder.try_decode_with_limit(&wire, usize::MAX).unwrap();
                assert_eq!(decoded, fields);
                wire_blocks.push(wire);
            }
            TableAction::Decode(wire) => {
                let result = decoder.try_decode_with_limit(wire, usize::MAX);
                wire_blocks.push(wire.clone());
                match result {
                    Ok(_) => {}
                    Err(observed) => {
                        error = Some(protocol_category(observed));
                    }
                }
            }
        }
    }

    let state = match input.state_owner {
        TableStateOwner::Decoder => decoder_table(&decoder),
        TableStateOwner::EncoderAndDecoder => {
            let encoder_state = encoder_table(&mut encoder);
            let decoder_state = decoder_table(&decoder);
            assert_eq!(encoder_state.max_size, decoder_state.max_size);
            assert_eq!(encoder_state.size, decoder_state.size);
            assert_eq!(encoder_state.entries, decoder_state.entries);
            encoder_state
        }
    };
    ActualEvidence::Table(TableActualEvidence {
        wire_blocks,
        error,
        capacity: state.max_size,
        entries: observed_table_entries(state.entries),
        accounted_size: state.size,
        container_entries: state.container_capacities[0],
        disposition: if error.is_some() {
            Disposition::Poisoned
        } else {
            Disposition::Reusable
        },
    })
}

#[cfg(feature = "hpack-test-support")]
fn execute_limit(case: &CaseDefinition) -> ActualEvidence {
    assert_eq!(case.initial_state, InitialState::FreshDefault);
    let CaseInput::Limit(input) = &case.input else {
        unreachable!()
    };
    let mut decoder = H2HeaderBlockDecoder::new();
    let mut accounting = None;
    let mut post_limit_allocations = Applicability::NotApplicable;
    let outcome = match &input.operation {
        LimitOperation::Decode { wire, limit } => {
            let outcome = decoder
                .try_decode_with_limit(wire, *limit)
                .map(|fields| fields.iter().map(observed_field).collect())
                .map_err(protocol_category);
            post_limit_allocations = observed_post_limit_allocations(&decoder);
            outcome
        }
        LimitOperation::EncodeThenDecode { fields, limit } => {
            let mut encoder = H2HeaderBlockEncoder::new();
            let fields = fields
                .iter()
                .map(|manifest_field| {
                    field(
                        manifest_field.name.as_slice(),
                        manifest_field.value.as_slice(),
                        manifest_field.sensitive,
                    )
                })
                .collect::<Vec<_>>();
            let wire = encoder.try_encode_fields(&fields).unwrap();
            let outcome = decoder
                .try_decode_with_limit(&wire, *limit)
                .map(|fields| fields.iter().map(observed_field).collect())
                .map_err(protocol_category);
            post_limit_allocations = observed_post_limit_allocations(&decoder);
            outcome
        }
        LimitOperation::SynchronizedReuse {
            field: manifest_field,
            crossed_limit,
            reuse_limit,
        } => {
            let dynamic = field(
                manifest_field.name.as_slice(),
                manifest_field.value.as_slice(),
                manifest_field.sensitive,
            );
            let mut encoder = H2HeaderBlockEncoder::new();
            let first = encoder
                .try_encode_fields(std::slice::from_ref(&dynamic))
                .unwrap();
            let second = encoder
                .try_encode_fields(std::slice::from_ref(&dynamic))
                .unwrap();
            let crossed = decoder
                .try_decode_with_limit(&first, *crossed_limit)
                .map(|_| ())
                .map_err(protocol_category);
            assert_eq!(
                crossed,
                Err(ErrorCategory::HeaderListTooLarge),
                "synchronized-reuse setup must cross the decoded limit"
            );
            post_limit_allocations = observed_post_limit_allocations(&decoder);
            assert_eq!(crossed.unwrap_err(), ErrorCategory::HeaderListTooLarge);
            decoder
                .try_decode_with_limit(&second, *reuse_limit)
                .map(|fields| fields.iter().map(observed_field).collect())
                .map_err(protocol_category)
        }
        LimitOperation::Accounting {
            name_len,
            value_len,
            limit,
            initial_total,
        } => {
            let (total, oversized) = account_lengths(*name_len, *value_len, *limit, *initial_total);
            accounting = Some(AccountingEvidence { total, oversized });
            Err(ErrorCategory::HeaderListTooLarge)
        }
        LimitOperation::PoisonRepeat {
            first_wire,
            second_wire,
            limit,
        } => {
            let first = decoder
                .try_decode_with_limit(first_wire, *limit)
                .unwrap_err();
            let first = protocol_category(first);
            let second = decoder
                .try_decode_with_limit(second_wire, *limit)
                .unwrap_err();
            assert_eq!(protocol_category(second), ErrorCategory::DecoderPoisoned);
            Err(first)
        }
        LimitOperation::AllocationInbound { .. } | LimitOperation::AllocationOutbound { .. } => {
            unreachable!("allocation cases belong to hpack_api")
        }
    };
    let disposition = match outcome {
        Err(
            ErrorCategory::Integer
            | ErrorCategory::String
            | ErrorCategory::InvalidIndex
            | ErrorCategory::InvalidMaximum
            | ErrorCategory::InvalidTableUpdate
            | ErrorCategory::TableUpdateAfterField
            | ErrorCategory::InvalidHuffman,
        ) => Disposition::Poisoned,
        _ => Disposition::Reusable,
    };
    let diagnostics = decoder_diagnostics(&decoder);
    let state = decoder_table(&decoder);
    ActualEvidence::Limit(LimitEvidence {
        outcome,
        disposition,
        table_entries: state
            .entries
            .into_iter()
            .map(|(name, value)| Field {
                name,
                value,
                sensitive: false,
            })
            .collect(),
        header_list_too_large_delta: diagnostics.header_list_too_large,
        compression_error_delta: diagnostics.compression_errors,
        accounting,
        post_limit_allocations,
    })
}

#[cfg(feature = "hpack-test-support")]
fn execute_corpus(input: &support::hpack_manifest::CorpusInput) -> ActualEvidence {
    let mut encoder = H2HeaderBlockEncoder::new();
    let mut decoder = H2HeaderBlockDecoder::new();
    let mut baseline_decoder = H2HeaderBlockDecoder::new();
    let mut candidate_wire = Vec::new();
    let mut baseline_wire = Vec::new();
    for step in &input.steps {
        for capacity in &step.capacity_updates {
            encoder.set_max_table_size(*capacity);
        }
        if let Some(capacity) = step.capacity_updates.last() {
            decoder.set_max_table_size(*capacity);
            baseline_decoder.set_max_table_size(*capacity);
        }
        let fields = step
            .fields
            .iter()
            .map(|manifest_field| {
                field(
                    manifest_field.name.as_slice(),
                    manifest_field.value.as_slice(),
                    manifest_field.sensitive,
                )
            })
            .collect::<Vec<_>>();
        let candidate = encoder.try_encode_fields(&fields).unwrap();
        assert_eq!(
            decoder
                .try_decode_with_limit(&candidate, usize::MAX)
                .unwrap(),
            *fields
        );
        candidate_wire.push(candidate);

        let mut baseline = Vec::new();
        if let Some(minimum) = step.capacity_updates.iter().copied().min() {
            push_integer(&mut baseline, minimum, 5, 0x20);
            let final_size = *step.capacity_updates.last().unwrap();
            if final_size != minimum {
                push_integer(&mut baseline, final_size, 5, 0x20);
            }
        }
        for manifest_field in &step.fields {
            baseline.push(if manifest_field.sensitive { 0x10 } else { 0 });
            push_integer(&mut baseline, manifest_field.name.len(), 7, 0);
            baseline.extend_from_slice(&manifest_field.name);
            push_integer(&mut baseline, manifest_field.value.len(), 7, 0);
            baseline.extend_from_slice(&manifest_field.value);
        }
        assert_eq!(
            baseline_decoder
                .try_decode_with_limit(&baseline, usize::MAX)
                .unwrap(),
            fields
        );
        baseline_wire.push(baseline);
    }
    assert!(
        candidate_wire.iter().map(Vec::len).sum::<usize>()
            < baseline_wire.iter().map(Vec::len).sum()
    );
    ActualEvidence::Corpus {
        candidate_wire,
        baseline_wire,
        baseline_domain_count: input.baseline_domain_count,
    }
}

#[cfg(feature = "hpack-test-support")]
fn execute_case(case: CaseDefinition) -> RunnerResult {
    if matches!(case.input, CaseInput::Corpus(_)) {
        assert_eq!(case.initial_state, InitialState::FreshDefault);
    }
    let actual = match &case.input {
        CaseInput::Integer(_) => execute_integer(&case),
        CaseInput::Huffman(_) => execute_huffman(&case),
        CaseInput::Representation(_) => execute_representation(&case),
        CaseInput::TableCapacity { .. } => execute_table_capacity(&case),
        CaseInput::TableTransition(_) => execute_table_transition(&case),
        CaseInput::Limit(input)
            if matches!(
                input.operation,
                LimitOperation::AllocationInbound { .. }
                    | LimitOperation::AllocationOutbound { .. }
            ) =>
        {
            unreachable!("allocation cases belong to hpack_api")
        }
        CaseInput::Limit(_) => execute_limit(&case),
        CaseInput::Corpus(input) => execute_corpus(input),
        _ => unreachable!("non-vector case {}", case.id),
    };
    RunnerResult {
        id: case.id,
        input: case.input,
        initial_state: case.initial_state,
        actual,
    }
}

#[cfg(feature = "hpack-test-support")]
#[test]
fn authoritative_phase_one_vector_results_complete_exactly_once() {
    let mut completion = CompletionLedger::phase_one(Runner::HpackVectors);
    let cases = completion.cases().cloned().collect::<Vec<_>>();
    for case in cases {
        completion.complete(execute_case(case));
    }
    completion.finish();
}

#[test]
fn compatibility_encoder_and_decoder_forms_remain_usable() {
    let fields = [kimojio_fsm_http::H2RawHeader::new(b"x", b"value")];
    const ENCODER: H2HeaderBlockEncoder = H2HeaderBlockEncoder::new();
    let mut encoder = ENCODER;
    let wire: Vec<u8> = encoder.encode(&fields);
    let decoded: Result<Vec<kimojio_fsm_http::H2RawHeader>, _> =
        H2HeaderBlockDecoder::new().decode_with_limit(&wire, usize::MAX);
    assert_eq!(decoded.unwrap(), fields);
}
