#![cfg(feature = "hpack-test-support")]

use kimojio_fsm_http::hpack_test_support::{
    client_outbound_table, saturate_client_hpack_diagnostics, server_inbound_table,
};
use kimojio_fsm_http::{
    H2ByteClientEvent, H2Client, H2Frame, H2FrameOutcome, H2FrameType, H2HeaderField,
    H2HpackDiagnosticsSnapshot, H2Limits, H2Server, H2Setting, H2SettingId, H2Settings,
    ServerError,
};

mod support;

use support::hpack_execution_ledger::{CompletionLedger, Runner, RunnerResult};
use support::hpack_manifest::{
    ActualEvidence, CaseInput, ConnectionBehavior, ConnectionEvidenceV3, ConnectionOutcome,
    ConnectionScenarioV3, ConnectionState, DirectionalDiagnosticsEvidenceV3,
    EffectivenessEvidenceV3, ErrorCategory, HpackDiagnosticsEvidenceV3,
};

fn settings_frame(settings: &[H2Setting]) -> H2Frame {
    let mut payload = Vec::new();
    H2Settings::encode_payload(settings, &mut payload);
    H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0,
        stream_id: 0,
        payload,
    }
}

fn initialize_client(client: &mut H2Client) {
    assert!(matches!(
        client.accept_frame_bytes_typed(settings_frame(&[])).0,
        H2FrameOutcome::Event(H2ByteClientEvent::Settings { .. })
    ));
}

fn take_header_block(client: &mut H2Client, commit: kimojio_fsm_http::H2OutboundCommit) -> Vec<u8> {
    let bytes = client
        .next_outbound_block()
        .expect("queued diagnostics output")
        .bytes()
        .to_vec();
    client.acknowledge_outbound_block(commit).unwrap();
    let mut cursor = 0;
    let mut block = Vec::new();
    while cursor < bytes.len() {
        let (frame, consumed) = H2Frame::decode(&bytes[cursor..]).unwrap();
        cursor += consumed;
        block.extend_from_slice(&frame.payload);
        if frame.flags & 0x4 != 0 {
            break;
        }
    }
    block
}

fn snapshot(value: H2HpackDiagnosticsSnapshot) -> HpackDiagnosticsEvidenceV3 {
    HpackDiagnosticsEvidenceV3 {
        encoded_blocks: value.encoded_blocks,
        decoded_blocks: value.decoded_blocks,
        indexed_fields: value.indexed_fields,
        incremental_fields: value.incremental_fields,
        without_indexing_fields: value.without_indexing_fields,
        never_indexed_fields: value.never_indexed_fields,
        huffman_strings: value.huffman_strings,
        plain_strings: value.plain_strings,
        table_size_updates: value.table_size_updates,
        table_insertions: value.table_insertions,
        table_evictions: value.table_evictions,
        compression_errors: value.compression_errors,
        local_limit_failures: value.local_limit_failures,
        field_octets: value.field_octets,
        wire_octets: value.wire_octets,
    }
}

fn effectiveness(value: H2HpackDiagnosticsSnapshot) -> EffectivenessEvidenceV3 {
    value
        .effectiveness()
        .map_or(EffectivenessEvidenceV3::Unavailable, |effectiveness| {
            EffectivenessEvidenceV3::Exact {
                encoded_wire_octets: effectiveness.encoded_wire_octets,
                uncompressed_field_octets: effectiveness.uncompressed_field_octets,
            }
        })
}

fn directional(
    inbound: H2HpackDiagnosticsSnapshot,
    outbound: H2HpackDiagnosticsSnapshot,
) -> DirectionalDiagnosticsEvidenceV3 {
    DirectionalDiagnosticsEvidenceV3 {
        inbound: snapshot(inbound),
        outbound: snapshot(outbound),
        inbound_effectiveness: effectiveness(inbound),
        outbound_effectiveness: effectiveness(outbound),
    }
}

fn diagnostics_evidence(
    behavior: ConnectionBehavior,
    expected_blocks: &[Vec<u8>],
) -> ConnectionEvidenceV3 {
    match behavior {
        ConnectionBehavior::DiagnosticsSuccess => {
            let mut client = H2Client::default();
            initialize_client(&mut client);
            let first_field = H2HeaderField::new(b"x-manifest", b"value");
            let first_commit = client
                .trailers_frame_with_raw_headers(1, std::slice::from_ref(&first_field))
                .unwrap();
            let first = take_header_block(&mut client, first_commit);
            client
                .accept_frame_bytes(settings_frame(&[H2Setting::new(
                    H2SettingId::HeaderTableSize,
                    0,
                )]))
                .unwrap();
            let second_fields = [
                H2HeaderField::new(b":method", b"GET"),
                H2HeaderField::new(b"x", [0xff]),
                H2HeaderField::new(b"authorization", [0xff]).with_sensitive(true),
            ];
            let second_commit = client
                .trailers_frame_with_raw_headers(3, &second_fields)
                .unwrap();
            let second = take_header_block(&mut client, second_commit);
            assert_eq!([first.clone(), second.clone()], expected_blocks);

            let mut server = H2Server::default();
            server.discard_hpack_block(&first).unwrap();
            server.discard_hpack_block(&second).unwrap();
            let diagnostics = directional(
                server.inbound_hpack_diagnostics(),
                client.outbound_hpack_diagnostics(),
            );
            ConnectionEvidenceV3 {
                outcomes: vec![
                    ConnectionOutcome::Wire(first),
                    ConnectionOutcome::Wire(second),
                ],
                occurrences: Vec::new(),
                inbound_entries: server_inbound_table(&server).entries.len(),
                outbound_entries: client_outbound_table(&client).entries.len(),
                state: ConnectionState::Open,
                reusable: true,
                commit_sequences: vec![first_commit.sequence(), second_commit.sequence()],
                diagnostics: Some(diagnostics),
            }
        }
        ConnectionBehavior::DiagnosticsLimit => {
            let mut server = H2Server::with_limits(H2Limits {
                max_header_list_size: 41,
                ..H2Limits::default()
            })
            .unwrap();
            assert_eq!(
                server.discard_hpack_block(&[0x82]),
                Err(ServerError::HeaderTooLarge {
                    limit: 41,
                    actual: 42
                })
            );
            ConnectionEvidenceV3 {
                outcomes: vec![ConnectionOutcome::Error(ErrorCategory::HeaderListTooLarge)],
                occurrences: Vec::new(),
                inbound_entries: 0,
                outbound_entries: 0,
                state: ConnectionState::Open,
                reusable: true,
                commit_sequences: Vec::new(),
                diagnostics: Some(directional(
                    server.inbound_hpack_diagnostics(),
                    H2HpackDiagnosticsSnapshot::default(),
                )),
            }
        }
        ConnectionBehavior::DiagnosticsError => {
            let mut server = H2Server::default();
            assert_eq!(
                server.discard_hpack_block(&[0x80]),
                Err(ServerError::InvalidHpack)
            );
            assert!(kimojio_fsm_http::hpack_test_support::server_connection_is_terminal(&server));
            let inbound = server.inbound_hpack_diagnostics();
            assert_eq!(
                server.discard_hpack_block(&[0x82]),
                Err(ServerError::InvalidHpack)
            );
            assert_eq!(server.inbound_hpack_diagnostics(), inbound);
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
                diagnostics: Some(directional(inbound, H2HpackDiagnosticsSnapshot::default())),
            }
        }
        ConnectionBehavior::DiagnosticsSaturation => {
            let mut client = H2Client::default();
            saturate_client_hpack_diagnostics(&mut client);
            ConnectionEvidenceV3 {
                outcomes: vec![ConnectionOutcome::Wire(Vec::new())],
                occurrences: Vec::new(),
                inbound_entries: 0,
                outbound_entries: 0,
                state: ConnectionState::Open,
                reusable: true,
                commit_sequences: Vec::new(),
                diagnostics: Some(directional(
                    client.inbound_hpack_diagnostics(),
                    client.outbound_hpack_diagnostics(),
                )),
            }
        }
        _ => unreachable!("not a diagnostics behavior"),
    }
}

#[test]
fn directional_snapshots_are_atomic_content_free_and_exactly_reduced() {
    let field = H2HeaderField::new(b"x-secret-name", b"secret-value");
    let mut client = H2Client::default();
    let before = client.outbound_hpack_diagnostics();
    assert_eq!(before, Default::default());
    let commit = client
        .trailers_frame_with_raw_headers(1, std::slice::from_ref(&field))
        .unwrap();
    client.acknowledge_outbound_block(commit).unwrap();
    let after = client.outbound_hpack_diagnostics();
    assert_eq!(after.encoded_blocks, 1);
    assert_eq!(after.field_octets, 25);
    assert!(after.wire_octets > 0);
    let effectiveness = after.effectiveness().unwrap();
    let divisor = (1..=after.wire_octets.min(25))
        .rev()
        .find(|candidate| after.wire_octets % candidate == 0 && 25 % candidate == 0)
        .unwrap();
    assert_eq!(
        effectiveness.encoded_wire_octets,
        after.wire_octets / divisor
    );
    assert_eq!(effectiveness.uncompressed_field_octets, 25 / divisor);
    let debug = format!("{after:?}");
    assert!(!debug.contains("x-secret-name"));
    assert!(!debug.contains("secret-value"));
    assert_eq!(client.inbound_hpack_diagnostics(), Default::default());
}

#[test]
fn failed_or_incomplete_blocks_commit_only_defined_deltas() {
    let mut server = H2Server::default();
    assert_eq!(
        server.discard_hpack_block(&[0x80]),
        Err(ServerError::InvalidHpack)
    );
    let failed = server.inbound_hpack_diagnostics();
    assert_eq!(failed.compression_errors, 1);
    assert_eq!(failed.decoded_blocks, 0);
    assert_eq!(failed.field_octets, 0);
    assert_eq!(failed.wire_octets, 0);

    let client = H2Client::default();
    assert_eq!(client.outbound_hpack_diagnostics(), Default::default());
}

#[test]
fn authoritative_phase_two_diagnostics_rows_complete_exactly_once() {
    let mut completion = CompletionLedger::phase_two(Runner::HpackDiagnostics);
    let cases = completion.cases().cloned().collect::<Vec<_>>();
    for case in cases {
        let CaseInput::ConnectionV3(ConnectionScenarioV3::Diagnostics { behavior, blocks }) =
            &case.input
        else {
            unreachable!("unexpected diagnostics case {}", case.id)
        };
        completion.complete(RunnerResult {
            id: case.id,
            input: case.input.clone(),
            initial_state: case.initial_state,
            actual: ActualEvidence::ConnectionV3(Box::new(diagnostics_evidence(*behavior, blocks))),
        });
    }
    completion.finish();
}
