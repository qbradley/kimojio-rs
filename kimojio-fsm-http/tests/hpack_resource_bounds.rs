use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[cfg(feature = "hpack-test-support")]
use kimojio_fsm_http::hpack_test_support::{
    decoder_configured_capacity, decoder_post_limit_allocations, encoder_configured_capacity,
};
#[cfg(feature = "hpack-test-support")]
use kimojio_fsm_http::hpack_test_support::{decoder_table, encoder_table};
#[cfg(feature = "hpack-test-support")]
use kimojio_fsm_http::{H2HeaderBlockDecoder, H2HpackError};
use kimojio_fsm_http::{H2HeaderBlockEncoder, H2HeaderField, H2RawHeaderRef};

mod support;

#[cfg(feature = "hpack-test-support")]
use support::hpack_execution_ledger::{CompletionLedger, Runner, RunnerResult};
#[cfg(feature = "hpack-test-support")]
use support::hpack_manifest::{
    ActualEvidence, Applicability, CaseInput, Disposition, ErrorCategory, InitialState,
    PostLimitAllocationEvidence, ResourceActualEvidence, ResourceInput, ResourceOperation,
    ResourceOutcome, ResourceScenario,
};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static DEALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static MEASUREMENT_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ACTIVE.get() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if ACTIVE.get() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if ACTIVE.get() {
            DEALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if ACTIVE.get() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn measure<T>(operation: impl FnOnce() -> T) -> (T, usize, usize) {
    let _guard = MEASUREMENT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ALLOCATIONS.store(0, Ordering::Relaxed);
    DEALLOCATIONS.store(0, Ordering::Relaxed);
    assert!(!ACTIVE.replace(true));
    let result = operation();
    ACTIVE.set(false);
    (
        result,
        ALLOCATIONS.load(Ordering::Relaxed),
        DEALLOCATIONS.load(Ordering::Relaxed),
    )
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

#[test]
fn sensitive_borrowed_field_has_constant_allocation_bound_and_no_table_copy() {
    let field = H2RawHeaderRef::new(b"authorization", b"a-secret-value").with_sensitive(true);
    let mut encoder = H2HeaderBlockEncoder::new();
    let (encoded, allocations, _) = measure(|| encoder.try_encode_ref(&[field]).unwrap());
    assert!(allocations <= 3, "unexpected allocations: {allocations}");
    assert_eq!(encoded, encoder.try_encode_ref(&[field]).unwrap());
}

#[test]
fn persistent_preflight_does_not_scale_with_exact_hit_block_cardinality() {
    let mut encoder = H2HeaderBlockEncoder::new();
    encoder.set_max_table_size(64);
    let field = H2HeaderField::new(b"x", b"value");
    encoder
        .try_encode_fields(std::slice::from_ref(&field))
        .unwrap();
    let fields = vec![field; 4096];
    let (_, allocations, _) = measure(|| encoder.try_encode_fields(&fields).unwrap());
    assert!(
        allocations <= fields.len() + 8,
        "persistent preflight regressed: {allocations}"
    );
}

#[cfg(feature = "hpack-test-support")]
fn error_category(error: H2HpackError) -> ErrorCategory {
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
fn resource_error(error: kimojio_fsm_http::H2ProtocolError) -> ResourceOutcome {
    ResourceOutcome::Error(error_category(
        error
            .hpack_error
            .expect("HPACK resource errors carry a category"),
    ))
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
fn observed_disposition(decoder: &mut H2HeaderBlockDecoder) -> Disposition {
    match decoder.try_decode_with_limit(&[], usize::MAX) {
        Ok(fields) => {
            assert!(fields.is_empty());
            Disposition::Reusable
        }
        Err(error) => match error.hpack_error {
            Some(H2HpackError::DecoderPoisoned) => Disposition::Poisoned,
            Some(H2HpackError::AllocationFailed) => Disposition::AllocationTerminal,
            other => panic!("unexpected decoder reuse outcome: {other:?}"),
        },
    }
}

#[cfg(feature = "hpack-test-support")]
fn execute_resource(input: &ResourceInput) -> ActualEvidence {
    let capacity = input.capacity;
    let mut encoder = H2HeaderBlockEncoder::new();
    let mut decoder = H2HeaderBlockDecoder::new();
    encoder.set_max_table_size(capacity);
    decoder.set_max_table_size(capacity);
    let configured_encoder_capacity = encoder_configured_capacity(&mut encoder);
    let configured_decoder_capacity = decoder_configured_capacity(&decoder);
    let mut observed_deallocations = 0;
    let mut operation_allocations = Applicability::NotApplicable;
    let mut post_limit_allocations = Applicability::NotApplicable;

    let outcome = match &input.scenario {
        ResourceScenario::CapacityOnly => {
            let (wire, allocations, _) = measure(|| encoder.try_encode_fields(&[]).unwrap());
            operation_allocations = Applicability::Applicable(allocations);
            match decoder.try_decode_with_limit(&wire, usize::MAX) {
                Ok(fields) => {
                    assert!(fields.is_empty());
                    ResourceOutcome::Success
                }
                Err(error) => resource_error(error),
            }
        }
        ResourceScenario::Insertion {
            field: manifest_field,
        } => {
            let field = H2HeaderField::new(
                manifest_field.name.as_slice(),
                manifest_field.value.as_slice(),
            )
            .with_sensitive(manifest_field.sensitive);
            let wire = encoder
                .try_encode_fields(std::slice::from_ref(&field))
                .unwrap();
            match decoder.try_decode_with_limit(&wire, usize::MAX) {
                Ok(fields) => {
                    assert_eq!(fields, [field]);
                    ResourceOutcome::Success
                }
                Err(error) => resource_error(error),
            }
        }
        ResourceScenario::Eviction {
            insertions,
            wire_entry,
        } => {
            let mut wire = Vec::with_capacity(insertions * wire_entry.len() + 8);
            push_integer(&mut wire, capacity, 5, 0x20);
            for _ in 0..*insertions {
                wire.extend_from_slice(wire_entry);
            }
            match decoder.try_decode_with_limit(&wire, usize::MAX) {
                Ok(fields) => {
                    assert_eq!(fields.len(), *insertions);
                    ResourceOutcome::Success
                }
                Err(error) => resource_error(error),
            }
        }
        ResourceScenario::Resize {
            field: manifest_field,
            target_capacity,
        } => {
            let field = H2HeaderField::new(
                manifest_field.name.as_slice(),
                manifest_field.value.as_slice(),
            )
            .with_sensitive(manifest_field.sensitive);
            let wire = encoder
                .try_encode_fields(std::slice::from_ref(&field))
                .unwrap();
            decoder.try_decode_with_limit(&wire, usize::MAX).unwrap();
            encoder.set_max_table_size(*target_capacity);
            decoder.set_max_table_size(*target_capacity);
            let wire = encoder.try_encode_fields(&[]).unwrap();
            match decoder.try_decode_with_limit(&wire, usize::MAX) {
                Ok(fields) => {
                    assert!(fields.is_empty());
                    ResourceOutcome::Success
                }
                Err(error) => resource_error(error),
            }
        }
        ResourceScenario::Clear {
            field: manifest_field,
            target_capacity,
        } => {
            let field = H2HeaderField::new(
                manifest_field.name.as_slice(),
                manifest_field.value.as_slice(),
            )
            .with_sensitive(manifest_field.sensitive);
            let wire = encoder
                .try_encode_fields(std::slice::from_ref(&field))
                .unwrap();
            decoder.try_decode_with_limit(&wire, usize::MAX).unwrap();
            encoder.set_max_table_size(*target_capacity);
            decoder.set_max_table_size(*target_capacity);
            let (wire, _, deallocations) = measure(|| encoder.try_encode_fields(&[]).unwrap());
            observed_deallocations = deallocations;
            match decoder.try_decode_with_limit(&wire, usize::MAX) {
                Ok(fields) => {
                    assert!(fields.is_empty());
                    ResourceOutcome::Success
                }
                Err(error) => resource_error(error),
            }
        }
        ResourceScenario::CrossedLimitDiscard {
            field: manifest_field,
            limit,
        } => {
            let field = H2HeaderField::new(
                manifest_field.name.as_slice(),
                manifest_field.value.as_slice(),
            )
            .with_sensitive(manifest_field.sensitive);
            let wire = encoder
                .try_encode_fields(std::slice::from_ref(&field))
                .unwrap();
            let outcome = match decoder.try_decode_with_limit(&wire, *limit) {
                Ok(_) => ResourceOutcome::Success,
                Err(error) => resource_error(error),
            };
            post_limit_allocations = observed_post_limit_allocations(&decoder);
            outcome
        }
        ResourceScenario::MalformedInteger {
            continuation_octet,
            continuation_count,
        } => {
            let mut wire = Vec::new();
            push_integer(&mut wire, capacity, 5, 0x20);
            wire.extend(std::iter::repeat_n(
                *continuation_octet,
                *continuation_count,
            ));
            match decoder.try_decode_with_limit(&wire, usize::MAX) {
                Ok(_) => ResourceOutcome::Success,
                Err(error) => resource_error(error),
            }
        }
        ResourceScenario::MalformedHuffman { literal } => {
            let mut wire = Vec::new();
            push_integer(&mut wire, capacity, 5, 0x20);
            wire.extend_from_slice(literal);
            match decoder.try_decode_with_limit(&wire, usize::MAX) {
                Ok(_) => ResourceOutcome::Success,
                Err(error) => resource_error(error),
            }
        }
    };
    let disposition = observed_disposition(&mut decoder);

    let state = if matches!(
        input.operation,
        ResourceOperation::Eviction
            | ResourceOperation::MalformedInteger
            | ResourceOperation::MalformedHuffman
    ) {
        decoder_table(&decoder)
    } else {
        let encoder_state = encoder_table(&mut encoder);
        let decoder_state = decoder_table(&decoder);
        assert_eq!(encoder_state.max_size, decoder_state.max_size);
        assert_eq!(encoder_state.size, decoder_state.size);
        assert_eq!(encoder_state.entries, decoder_state.entries);
        encoder_state
    };
    ActualEvidence::Resource(ResourceActualEvidence {
        configured_encoder_capacity,
        configured_decoder_capacity,
        operation: input.operation,
        outcome,
        final_capacity: state.max_size,
        table_entries: state.entries.len(),
        retained_size: state.size,
        container_capacities: state.container_capacities,
        observed_deallocations,
        operation_allocations,
        post_limit_allocations,
        disposition,
    })
}

#[cfg(feature = "hpack-test-support")]
#[test]
fn authoritative_resource_results_complete_exactly_once() {
    let mut completion = CompletionLedger::phase_one(Runner::HpackResourceBounds);
    let cases = completion.cases().cloned().collect::<Vec<_>>();
    for case in cases {
        let CaseInput::Resource(input) = &case.input else {
            unreachable!("non-resource row {}", case.id)
        };
        assert_eq!(
            case.initial_state,
            InitialState::FreshCapacity(input.capacity)
        );
        let actual = execute_resource(input);
        completion.complete(RunnerResult {
            id: case.id,
            input: case.input,
            initial_state: case.initial_state,
            actual,
        });
    }
    completion.finish();
}
