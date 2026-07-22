use std::collections::{BTreeMap, BTreeSet};

use kimojio_fsm_http::{H2HeaderBlockDecoder, H2RawHeader};
use sha2::{Digest, Sha256};

mod support;

use support::hpack_execution_ledger::{
    CompletionLedger, ExecutionState, Phase, Runner, RunnerResult, execution_ledger_v1,
    execution_ledger_v2, execution_ledger_v3, execution_ledger_v4,
};
use support::hpack_final_closure::{
    COMPATIBILITY_EVIDENCE_ENV, ClosureDocuments, FinalClosureRunner,
    complete_compatibility_evidence_for_negative_tests,
};
use support::hpack_manifest::{
    AcceptanceClass, ActualEvidence, Applicability, CURRENT_MANIFEST_VERSION, CaseInput,
    DirectionalDiagnosticsEvidenceV3, EffectivenessEvidenceV3, ExpectedEvidence, InitialState,
    MANIFEST_V2_ORACLE_CORRECTIONS, MANIFEST_V2_SUPERSEDES_VERSION, MANIFEST_V3_SUPERSEDES_VERSION,
    MANIFEST_V4_SUPERSEDES_VERSION, Observable, PHASE_FOUR_REQUIREMENTS, PHASE_ONE_REQUIREMENTS,
    PHASE_THREE_REQUIREMENTS, PHASE_TWO_REQUIREMENTS, PostLimitAllocationEvidence,
    ResourceActualEvidence, ResourceOutcome, manifest_v1, manifest_v2, manifest_v3, manifest_v4,
    required_observables,
};

const RFC_REQUEST_HEX: &str = include_str!("assets/hpack/rfc7541-c4-request-huffman-v1.hex");
const RFC_REQUEST_SHA256: &str = "da87a186aae725a570c0674360ec9676274abd77d6af3b726de03314eae523c4";
const MANIFEST_V1_SHA256: &str = "7f1466bc2f42769f80039646731c25dcff4fb08239e68b1ec981ef9aa56638f5";
const MANIFEST_V2_SHA256: &str = "e057224dae126687aaad133ddd28ef617a909b527e6bac6017eb38b49773f464";
const MANIFEST_V3_SHA256: &str = "582c80c7d3baf7828cfd42855ae62b06ecc76cbf89bbe9a7277ad406b6c71c68";
const MANIFEST_V4_SHA256: &str = "0594b1f2777ad43492073b411e658d101d502425faeb44a7942cb3c6369f65f6";

fn decode_hex(input: &str) -> Vec<u8> {
    let input = input.trim().as_bytes();
    assert!(input.len().is_multiple_of(2));
    input
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).unwrap();
            u8::from_str_radix(text, 16).unwrap()
        })
        .collect()
}

fn canonical_manifest_bytes(manifest: impl IntoIterator<Item = impl std::fmt::Debug>) -> Vec<u8> {
    manifest
        .into_iter()
        .map(|case| format!("{case:?}\n"))
        .collect::<String>()
        .into_bytes()
}

#[test]
fn manifest_v1_definitions_are_unique_typed_complete_and_immutable() {
    let manifest = manifest_v1();
    let mut ids = BTreeSet::new();
    let mut classes = BTreeSet::new();
    for case in &manifest {
        assert!(case.id.ends_with("-v1"), "unversioned ID {}", case.id);
        assert!(ids.insert(&case.id), "duplicate ID {}", case.id);
        assert!(!case.requirement_links.is_empty(), "unlinked {}", case.id);
        classes.insert(case.class);
    }
    assert_eq!(ids.len(), manifest.len());
    assert_eq!(
        classes,
        BTreeSet::from([
            AcceptanceClass::Int,
            AcceptanceClass::Huff,
            AcceptanceClass::Rep,
            AcceptanceClass::Table,
            AcceptanceClass::Limit,
            AcceptanceClass::Connection,
            AcceptanceClass::Path,
            AcceptanceClass::Resource,
            AcceptanceClass::Corpus,
            AcceptanceClass::Api,
            AcceptanceClass::Closure,
        ])
    );
    let digest = format!("{:x}", Sha256::digest(canonical_manifest_bytes(&manifest)));
    assert_eq!(
        digest, MANIFEST_V1_SHA256,
        "typed manifest digest: {digest}"
    );
}

#[test]
fn manifest_v2_completely_inherits_v1_and_explicitly_supersedes_bad_oracles() {
    assert_eq!(MANIFEST_V2_SUPERSEDES_VERSION, 1);

    let v1 = manifest_v1()
        .into_iter()
        .map(|case| (case.id.clone(), case))
        .collect::<BTreeMap<_, _>>();
    let v2 = manifest_v2();
    assert_eq!(v2.len(), v1.len());
    let mut v2_ids = BTreeSet::new();
    let mut seen_corrections = BTreeSet::new();
    let mut corrected_v1_ids = BTreeSet::new();
    let mut corrected_v2_ids = BTreeSet::new();
    for correction in MANIFEST_V2_ORACLE_CORRECTIONS {
        assert!(corrected_v1_ids.insert(correction.superseded_id));
        assert!(corrected_v2_ids.insert(correction.replacement_id));
        assert!(!correction.reason.is_empty());
    }

    let corrected_wire = vec![
        0x40, 0x87, 0xf2, 0xb5, 0x23, 0xa8, 0xd2, 0x95, 0x09, 0x84, 0xee, 0x3a, 0x2d, 0x2f,
    ];
    let literal_wire = vec![
        0x40, 0x0a, b'x', b'-', b'm', b'a', b'n', b'i', b'f', b'e', b's', b't', 0x05, b'v', b'a',
        b'l', b'u', b'e',
    ];
    for current in &v2 {
        assert!(current.id.ends_with("-v2"), "unversioned ID {}", current.id);
        assert!(
            v2_ids.insert(current.id.as_str()),
            "duplicate ID {}",
            current.id
        );
        let superseded_id = format!(
            "{}-v1",
            current.id.strip_suffix("-v2").expect("Manifest v2 case ID")
        );
        let superseded = &v1[&superseded_id];
        assert_eq!(current.class, superseded.class);
        assert_eq!(current.input, superseded.input);
        assert_eq!(current.initial_state, superseded.initial_state);
        assert_eq!(current.requirement_links, superseded.requirement_links);
        if corrected_v1_ids.contains(superseded_id.as_str()) {
            assert!(corrected_v2_ids.contains(current.id.as_str()));
            assert!(seen_corrections.insert(current.id.as_str()));
            let ExpectedEvidence::Connection(previous) = &superseded.expected else {
                panic!("superseded row must contain connection evidence")
            };
            let ExpectedEvidence::Connection(replacement) = &current.expected else {
                panic!("replacement row must contain connection evidence")
            };
            assert_eq!(
                previous.outcome,
                support::hpack_manifest::ConnectionOutcome::Wire(literal_wire.clone())
            );
            assert_eq!(
                replacement.outcome,
                support::hpack_manifest::ConnectionOutcome::Wire(corrected_wire.clone())
            );
        } else {
            assert_eq!(current.expected, superseded.expected);
        }
    }
    assert_eq!(
        seen_corrections,
        MANIFEST_V2_ORACLE_CORRECTIONS
            .iter()
            .map(|correction| correction.replacement_id)
            .collect()
    );
    let digest = format!("{:x}", Sha256::digest(canonical_manifest_bytes(&v2)));
    assert_eq!(
        digest, MANIFEST_V2_SHA256,
        "typed manifest digest: {digest}"
    );
}

#[test]
fn manifest_v3_completely_supersedes_v2_without_rewriting_it() {
    assert_eq!(MANIFEST_V3_SUPERSEDES_VERSION, 2);
    let v3 = manifest_v3();
    let mut ids = BTreeSet::new();
    for case in &v3 {
        assert!(case.id.ends_with("-v3"));
        assert!(ids.insert(case.id.clone()), "duplicate {}", case.id);
        if case.class == AcceptanceClass::Connection {
            assert_ne!(case.initial_state, InitialState::ScenarioDefined);
            assert!(matches!(case.input, CaseInput::ConnectionV3(_)));
            assert!(matches!(case.expected, ExpectedEvidence::ConnectionV3(_)));
        }
    }
    for required in [
        "CONNECTION-SERVER-COMPLETE-ENCODED-ONE-OVER-v3",
        "CONNECTION-CLIENT-COMPLETE-ENCODED-ONE-OVER-v3",
        "CONNECTION-SERVER-ENCODED-ACCOUNTING-OVERFLOW-v3",
        "CONNECTION-CLIENT-ENCODED-ACCOUNTING-OVERFLOW-v3",
        "CONNECTION-SERVER-ASSEMBLY-ALLOCATION-v3",
        "CONNECTION-CLIENT-ASSEMBLY-ALLOCATION-v3",
    ] {
        assert!(ids.contains(required), "missing {required}");
    }
    let digest = format!("{:x}", Sha256::digest(canonical_manifest_bytes(&v3)));
    assert_eq!(
        digest, MANIFEST_V3_SHA256,
        "typed manifest digest: {digest}"
    );
}

#[test]
fn manifest_v4_supersedes_v3_with_complete_phase_three_oracles() {
    assert_eq!(CURRENT_MANIFEST_VERSION, 4);
    assert_eq!(MANIFEST_V4_SUPERSEDES_VERSION, 3);
    let v4 = manifest_v4();
    assert!(v4.iter().all(|case| case.id.ends_with("-v4")));
    for case in v4.iter().filter(|case| case.class == AcceptanceClass::Path) {
        assert_ne!(case.initial_state, InitialState::ScenarioDefined);
        assert!(matches!(case.input, CaseInput::PathV4(_)));
        assert!(matches!(case.expected, ExpectedEvidence::PathV4(_)));
    }
    for required in [
        "PATH-05-PSEUDO-REQUEST-v4",
        "PATH-05-PSEUDO-RESPONSE-v4",
        "PATH-05-PSEUDO-TRAILERS-REJECT-v4",
        "PATH-07-LEGAL-NON-TEXT-v4",
        "PATH-07-MIXED-ORDERED-v4",
    ] {
        assert!(
            v4.iter().any(|case| case.id == required),
            "missing {required}"
        );
    }
    let digest = format!("{:x}", Sha256::digest(canonical_manifest_bytes(&v4)));
    assert_eq!(
        digest, MANIFEST_V4_SHA256,
        "typed manifest digest: {digest}"
    );
}

#[test]
fn current_execution_ledger_uses_v4_as_its_sole_denominator() {
    let historical_manifest_ids = manifest_v1()
        .into_iter()
        .map(|case| case.id)
        .collect::<BTreeSet<_>>();
    let historical_ledger_ids = execution_ledger_v1()
        .into_iter()
        .map(|row| row.id)
        .collect::<BTreeSet<_>>();
    assert_eq!(historical_ledger_ids, historical_manifest_ids);
    assert_eq!(
        execution_ledger_v2()
            .into_iter()
            .map(|row| row.id)
            .collect::<BTreeSet<_>>(),
        manifest_v2()
            .into_iter()
            .map(|case| case.id)
            .collect::<BTreeSet<_>>()
    );

    assert_eq!(
        execution_ledger_v3()
            .into_iter()
            .map(|row| row.id)
            .collect::<BTreeSet<_>>(),
        manifest_v3()
            .into_iter()
            .map(|case| case.id)
            .collect::<BTreeSet<_>>()
    );

    let manifest_ids = manifest_v4()
        .into_iter()
        .map(|case| case.id)
        .collect::<BTreeSet<_>>();
    let ledger = execution_ledger_v4();
    let mut ledger_ids = BTreeSet::new();
    let mut owned = BTreeMap::new();
    for row in ledger {
        assert!(row.id.ends_with("-v4"));
        assert!(ledger_ids.insert(row.id.clone()), "duplicate ledger row");
        assert!(
            owned
                .insert(row.id.clone(), (row.phase, row.runner))
                .is_none()
        );
        assert_eq!(row.state, ExecutionState::Active);
    }
    assert_eq!(ledger_ids, manifest_ids);
}

#[test]
fn phase_one_reverse_coverage_is_closed_by_executable_rows() {
    assert_eq!(
        required_observables("FR-009-success-delta"),
        &[Observable::Diagnostics]
    );
    assert_eq!(
        required_observables("NFR-001-crossed-limit-no-output-allocation"),
        &[
            Observable::DiscardedOutputAllocation,
            Observable::DynamicTableSynchronizationAllocation,
        ]
    );
    let phase_one_ids = execution_ledger_v4()
        .into_iter()
        .filter(|row| row.phase == Phase::One)
        .map(|row| row.id)
        .collect::<BTreeSet<_>>();
    let mut coverage = BTreeMap::<&str, usize>::new();
    for case in manifest_v4()
        .into_iter()
        .filter(|case| phase_one_ids.contains(&case.id))
    {
        assert_ne!(
            case.initial_state,
            InitialState::ScenarioDefined,
            "Phase 1 row {} has a placeholder initial state",
            case.id
        );
        for requirement in case.requirement_links {
            for observable in required_observables(requirement) {
                assert!(
                    case.expected.declares_observable(*observable),
                    "{} links {requirement} without exact {observable:?} evidence",
                    case.id,
                );
            }
            *coverage.entry(requirement).or_default() += 1;
        }
    }

    let missing = PHASE_ONE_REQUIREMENTS
        .iter()
        .copied()
        .filter(|requirement| !coverage.contains_key(requirement))
        .collect::<Vec<_>>();
    assert!(missing.is_empty(), "uncovered requirements: {missing:?}");
}

#[test]
fn phase_two_rows_are_active_and_reverse_coverage_is_closed() {
    let rows = execution_ledger_v4()
        .into_iter()
        .filter(|row| row.phase == Phase::Two)
        .collect::<Vec<_>>();
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|row| row.state == ExecutionState::Active));
    let ids = rows
        .iter()
        .map(|row| row.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut coverage = BTreeMap::<&str, usize>::new();
    for case in manifest_v4()
        .into_iter()
        .filter(|case| ids.contains(case.id.as_str()))
    {
        assert_ne!(case.initial_state, InitialState::ScenarioDefined);
        for requirement in case.requirement_links {
            for observable in required_observables(requirement) {
                assert!(
                    case.expected.declares_observable(*observable),
                    "{} links {requirement} without {observable:?}",
                    case.id
                );
            }

            *coverage.entry(requirement).or_default() += 1;
        }
    }
    for requirement in PHASE_TWO_REQUIREMENTS {
        assert!(coverage.contains_key(requirement), "missing {requirement}");
    }
}

#[test]
fn phase_three_paths_are_active_exactly_owned_and_reverse_covered() {
    let rows = execution_ledger_v4()
        .into_iter()
        .filter(|row| row.phase == Phase::Three)
        .collect::<Vec<_>>();
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|row| row.state == ExecutionState::Active));
    let ids = rows
        .iter()
        .map(|row| row.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut coverage = BTreeMap::<&str, usize>::new();
    for case in manifest_v4()
        .into_iter()
        .filter(|case| ids.contains(case.id.as_str()))
    {
        assert_ne!(case.initial_state, InitialState::ScenarioDefined);
        assert!(matches!(case.input, CaseInput::PathV4(_)));
        assert!(matches!(case.expected, ExpectedEvidence::PathV4(_)));
        for requirement in case.requirement_links {
            for observable in required_observables(requirement) {
                assert!(
                    case.expected.declares_observable(*observable),
                    "{} links {requirement} without {observable:?}",
                    case.id
                );
            }
            *coverage.entry(requirement).or_default() += 1;
        }
    }
    for requirement in PHASE_THREE_REQUIREMENTS {
        assert!(coverage.contains_key(requirement), "missing {requirement}");
    }
    assert!(rows.iter().all(|row| match row.runner {
        Runner::FsmHttpAdapters => {
            row.id.starts_with("PATH-01")
                || row.id.starts_with("PATH-02")
                || row.id.starts_with("PATH-03")
                || row.id.starts_with("PATH-04")
                || row.id.starts_with("PATH-08")
        }
        Runner::StackHttpAdapters => row.id.starts_with("PATH-05"),
        Runner::FsmGrpcMetadata => row.id.starts_with("PATH-06"),
        Runner::StackGrpcMetadata => row.id.starts_with("PATH-07"),
        _ => false,
    }));
}

#[test]
fn phase_four_closure_requires_independently_observed_evidence() {
    let rows = execution_ledger_v4()
        .into_iter()
        .filter(|row| row.phase == Phase::Four)
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), PHASE_FOUR_REQUIREMENTS.len());
    assert!(rows.iter().all(|row| {
        row.state == ExecutionState::Active && row.runner == Runner::FinalCompatibility
    }));

    let ids = rows
        .iter()
        .map(|row| row.id.as_str())
        .collect::<BTreeSet<_>>();
    let expected_ids = (1..=PHASE_FOUR_REQUIREMENTS.len())
        .map(|criterion| format!("CLOSURE-SC-{criterion:03}-v4"))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        ids,
        expected_ids.iter().map(String::as_str).collect(),
        "Phase 4 closure ownership drift"
    );

    let mut coverage = BTreeMap::<&str, usize>::new();
    for case in manifest_v4()
        .into_iter()
        .filter(|case| ids.contains(case.id.as_str()))
    {
        assert!(matches!(case.input, CaseInput::Closure(_)));
        assert!(matches!(case.expected, ExpectedEvidence::Closure { .. }));
        assert!(case.expected.declares_observable(Observable::Aggregate));
        for requirement in case.requirement_links {
            *coverage.entry(requirement).or_default() += 1;
        }
    }
    assert_eq!(
        coverage,
        PHASE_FOUR_REQUIREMENTS
            .iter()
            .copied()
            .map(|requirement| (requirement, 1))
            .collect()
    );

    if std::env::var_os(COMPATIBILITY_EVIDENCE_ENV).is_none() {
        assert!(
            FinalClosureRunner::from_environment().is_err(),
            "closure completed without independently observed compatibility evidence"
        );
        return;
    }

    let runner = FinalClosureRunner::from_environment()
        .expect("compatibility runner must supply complete, valid evidence");
    let mut completion = CompletionLedger::phase_four(Runner::FinalCompatibility);
    for case in completion.cases().cloned().collect::<Vec<_>>() {
        completion.complete(
            runner
                .result_for(&case)
                .unwrap_or_else(|error| panic!("{} did not close: {error}", case.id)),
        );
    }
    completion.finish();
    println!("HPACK_FINAL_CLOSURE_COMPLETE 6/6");
}

fn phase_four_case(criterion: u8) -> support::hpack_manifest::CaseDefinition {
    manifest_v4()
        .into_iter()
        .find(|case| case.id == format!("CLOSURE-SC-{criterion:03}-v4"))
        .unwrap()
}

#[test]
fn phase_four_closure_rejects_missing_constituent_runner_results() {
    let evidence = complete_compatibility_evidence_for_negative_tests();
    for (criterion, missing) in [
        (1, "kimojio-fsm-http|default|test:hpack_vectors|test"),
        (
            2,
            "kimojio-stack-http|all-features|test:hpack_adapters|test",
        ),
        (3, "kimojio-fsm-http|default|test:hpack_connection|test"),
        (
            4,
            "kimojio-fsm-http|all-features|test:hpack_diagnostics|test",
        ),
    ] {
        let mutated = evidence
            .lines()
            .filter(|line| !line.ends_with(missing))
            .collect::<Vec<_>>()
            .join("\n");
        let runner = FinalClosureRunner::new(&mutated, ClosureDocuments::repository()).unwrap();
        assert!(
            runner.result_for(&phase_four_case(criterion)).is_err(),
            "SC-{criterion:03} accepted missing constituent runner {missing}"
        );
        assert!(
            runner.result_for(&phase_four_case(6)).is_err(),
            "SC-006 accepted missing prerequisite runner {missing}"
        );
    }
}

#[test]
fn phase_four_closure_rejects_compatibility_matrix_drift() {
    let evidence = complete_compatibility_evidence_for_negative_tests();
    let missing = evidence.lines().skip(1).collect::<Vec<_>>().join("\n");
    let runner = FinalClosureRunner::new(&missing, ClosureDocuments::repository()).unwrap();
    assert!(runner.result_for(&phase_four_case(5)).is_err());
    assert!(runner.result_for(&phase_four_case(6)).is_err());

    let mut wrong_status = evidence.clone();
    let passing = wrong_status
        .lines()
        .find(|line| line.starts_with("PASS "))
        .unwrap()
        .to_owned();
    let replacement = format!(
        "{} required-features=unexpected",
        passing.replacen("PASS ", "NOT_APPLICABLE ", 1)
    );
    wrong_status = wrong_status.replacen(&passing, &replacement, 1);
    let runner = FinalClosureRunner::new(&wrong_status, ClosureDocuments::repository()).unwrap();
    assert!(runner.result_for(&phase_four_case(5)).is_err());
}

#[test]
fn phase_four_closure_rejects_document_and_inventory_drift() {
    let evidence = complete_compatibility_evidence_for_negative_tests();
    let repository = ClosureDocuments::repository();
    let runner = FinalClosureRunner::new(&evidence, repository.clone()).unwrap();
    assert!(
        runner.result_for(&phase_four_case(6)).is_ok(),
        "repository closure fixture must satisfy SC-006"
    );

    for documents in [
        ClosureDocuments {
            technical_reference: "",
            ..repository.clone()
        },
        ClosureDocuments {
            fsm_http_guide: "",
            ..repository.clone()
        },
        ClosureDocuments {
            stack_http_grpc_guide: "",
            ..repository.clone()
        },
        ClosureDocuments {
            readme: "",
            ..repository.clone()
        },
        ClosureDocuments {
            crate_readme: "",
            ..repository.clone()
        },
        ClosureDocuments {
            inventory: "",
            ..repository.clone()
        },
    ] {
        let runner = FinalClosureRunner::new(&evidence, documents).unwrap();
        assert!(runner.result_for(&phase_four_case(6)).is_err());
    }

    let restored = format!(
        "{}\n| HPACK-005 | P1 | Medium | restored |\n",
        repository.inventory
    );
    let runner = FinalClosureRunner::new(
        &evidence,
        ClosureDocuments {
            inventory: &restored,
            ..repository.clone()
        },
    )
    .unwrap();
    assert!(runner.result_for(&phase_four_case(6)).is_err());

    let removed_unrelated = repository.inventory.replace("| TEST-002 |", "| REMOVED |");
    let runner = FinalClosureRunner::new(
        &evidence,
        ClosureDocuments {
            inventory: &removed_unrelated,
            ..repository
        },
    )
    .unwrap();
    assert!(runner.result_for(&phase_four_case(6)).is_err());
}

#[test]
fn phase_two_diagnostics_reject_every_counter_and_effectiveness_drift() {
    let case = manifest_v4()
        .into_iter()
        .find(|case| case.id == "CONNECTION-DIAGNOSTICS-SUCCESS-v4")
        .unwrap();
    let ExpectedEvidence::ConnectionV3(expected) = &case.expected else {
        unreachable!()
    };
    let mutations: &[fn(&mut DirectionalDiagnosticsEvidenceV3)] = &[
        |value| value.inbound.encoded_blocks ^= 1,
        |value| value.inbound.decoded_blocks ^= 1,
        |value| value.inbound.indexed_fields ^= 1,
        |value| value.inbound.incremental_fields ^= 1,
        |value| value.inbound.without_indexing_fields ^= 1,
        |value| value.inbound.never_indexed_fields ^= 1,
        |value| value.inbound.huffman_strings ^= 1,
        |value| value.inbound.plain_strings ^= 1,
        |value| value.inbound.table_size_updates ^= 1,
        |value| value.inbound.table_insertions ^= 1,
        |value| value.inbound.table_evictions ^= 1,
        |value| value.inbound.compression_errors ^= 1,
        |value| value.inbound.local_limit_failures ^= 1,
        |value| value.inbound.field_octets ^= 1,
        |value| value.inbound.wire_octets ^= 1,
        |value| value.outbound.encoded_blocks ^= 1,
        |value| value.outbound.decoded_blocks ^= 1,
        |value| value.outbound.indexed_fields ^= 1,
        |value| value.outbound.incremental_fields ^= 1,
        |value| value.outbound.without_indexing_fields ^= 1,
        |value| value.outbound.never_indexed_fields ^= 1,
        |value| value.outbound.huffman_strings ^= 1,
        |value| value.outbound.plain_strings ^= 1,
        |value| value.outbound.table_size_updates ^= 1,
        |value| value.outbound.table_insertions ^= 1,
        |value| value.outbound.table_evictions ^= 1,
        |value| value.outbound.compression_errors ^= 1,
        |value| value.outbound.local_limit_failures ^= 1,
        |value| value.outbound.field_octets ^= 1,
        |value| value.outbound.wire_octets ^= 1,
        |value| value.inbound_effectiveness = EffectivenessEvidenceV3::Unavailable,
        |value| value.outbound_effectiveness = EffectivenessEvidenceV3::Unavailable,
    ];
    for mutate in mutations {
        let mut actual = expected.clone();
        mutate(actual.diagnostics.as_mut().unwrap());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                case.expected
                    .assert_matches(&ActualEvidence::ConnectionV3(actual));
            }))
            .is_err()
        );
    }
}

#[test]
fn authoritative_completion_rejects_state_and_observable_drift() {
    let case = CompletionLedger::phase_one(Runner::HpackApi)
        .cases()
        .find(|case| matches!(case.input, CaseInput::Api(_)))
        .unwrap()
        .clone();
    let CaseInput::Api(input) = case.input.clone() else {
        unreachable!()
    };

    let state_drift = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut ledger = CompletionLedger::phase_one(Runner::HpackApi);
        ledger.complete(RunnerResult {
            id: case.id.clone(),
            input: case.input.clone(),
            initial_state: InitialState::ScenarioDefined,
            actual: ActualEvidence::Api {
                case: input.case,
                symbol: input.symbol,
            },
        });
    }));
    assert!(state_drift.is_err());

    let observable_drift = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut ledger = CompletionLedger::phase_one(Runner::HpackApi);
        ledger.complete(RunnerResult {
            id: case.id.clone(),
            input: case.input.clone(),
            initial_state: case.initial_state,
            actual: ActualEvidence::Api {
                case: input.case,
                symbol: "wrong-symbol",
            },
        });
    }));
    assert!(observable_drift.is_err());
}

#[test]
fn authoritative_completion_rejects_resource_outcome_and_allocation_drift() {
    let case = CompletionLedger::phase_one(Runner::HpackResourceBounds)
        .cases()
        .find(|case| case.id == "RESOURCE-4096-CROSSED-LIMIT-DISCARD-v4")
        .unwrap()
        .clone();
    let ExpectedEvidence::Resource(expected) = &case.expected else {
        unreachable!()
    };
    let actual = || ResourceActualEvidence {
        configured_encoder_capacity: expected.capacity,
        configured_decoder_capacity: expected.capacity,
        operation: expected.operation,
        outcome: expected.outcome,
        final_capacity: expected.final_capacity,
        table_entries: expected.table_entries,
        retained_size: expected.retained_size,
        container_capacities: [0; 3],
        observed_deallocations: expected.minimum_deallocations,
        operation_allocations: expected.operation_allocations,
        post_limit_allocations: expected.post_limit_allocations,
        disposition: expected.disposition,
    };

    let outcome_drift = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut actual = actual();
        actual.outcome = ResourceOutcome::Success;
        let mut ledger = CompletionLedger::phase_one(Runner::HpackResourceBounds);
        ledger.complete(RunnerResult {
            id: case.id.clone(),
            input: case.input.clone(),
            initial_state: case.initial_state,
            actual: ActualEvidence::Resource(actual),
        });
    }));
    assert!(outcome_drift.is_err());

    let allocation_drift = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut actual = actual();
        actual.post_limit_allocations = Applicability::Applicable(PostLimitAllocationEvidence {
            discarded_output_allocations: 1,
            dynamic_table_synchronization_allocations: 4,
        });
        let mut ledger = CompletionLedger::phase_one(Runner::HpackResourceBounds);
        ledger.complete(RunnerResult {
            id: case.id.clone(),
            input: case.input.clone(),
            initial_state: case.initial_state,
            actual: ActualEvidence::Resource(actual),
        });
    }));
    assert!(allocation_drift.is_err());
}

#[test]
fn named_partitions_and_boundaries_are_present() {
    let ids = manifest_v4()
        .into_iter()
        .map(|case| case.id)
        .collect::<BTreeSet<_>>();
    for prefix in [4, 5, 6, 7] {
        for boundary in ["below", "equal", "one-over"] {
            assert!(ids.contains(&format!("INT-P{prefix}-{boundary}-v4")));
        }
    }
    for first in 0..=u8::MAX {
        for second in 0..=u8::MAX {
            assert!(ids.contains(&format!("HUFF-PAIR-{first:02X}-{second:02X}-v4")));
        }
    }
    for capacity in [0, 4096, 65_535, 65_536, 65_537, 1_048_576] {
        for population in ["EMPTY", "ONE", "MAX-MINIMUM"] {
            assert!(ids.contains(&format!("TABLE-CAP-{capacity}-{population}-v4")));
        }
        for operation in [
            "INSERTION",
            "EVICTION",
            "RESIZE",
            "CLEAR",
            "CAPACITY-ONLY",
            "CROSSED-LIMIT-DISCARD",
            "MALFORMED-INTEGER",
            "MALFORMED-HUFFMAN",
        ] {
            assert!(ids.contains(&format!("RESOURCE-{capacity}-{operation}-v4")));
        }
    }
}

#[test]
fn checked_in_rfc_block_is_stable_and_decodes_exactly() {
    assert_eq!(
        format!("{:x}", Sha256::digest(RFC_REQUEST_HEX.as_bytes())),
        RFC_REQUEST_SHA256
    );
    let bytes = decode_hex(RFC_REQUEST_HEX);
    let mut decoder = H2HeaderBlockDecoder::new();
    assert_eq!(
        decoder.decode_with_limit(&bytes, usize::MAX).unwrap(),
        [
            H2RawHeader::new(":method", "GET"),
            H2RawHeader::new(":scheme", "http"),
            H2RawHeader::new(":path", "/"),
            H2RawHeader::new(":authority", "www.example.com"),
        ]
    );
}
