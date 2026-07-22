use kimojio_fsm_http::{
    CLIENT_PREFACE, H2ByteClientEvent, H2ByteStreamEvent, H2Client, H2Frame, H2FrameType,
    H2HeaderBlockDecoder, H2HeaderBlockEncoder, H2HeaderField, H2HeaderProjectionError,
    H2HeaderRole, H2HpackDiagnosticsSnapshot, H2Server, project_h2_header_fields,
    project_h2_header_fields_for_role,
};

mod support;

use support::hpack_execution_ledger::{CompletionLedger, Runner, RunnerResult};
use support::hpack_manifest::{
    ActualEvidence, AdapterOutcome, Applicability, CaseInput, Field, HpackDiagnosticsEvidenceV3,
    PathErrorV4, PathEvidenceV4, PathInput, PathRoleEvidenceV4, PathRoleV4, PathScenarioV4,
};

fn h2_fields(fields: &[Field]) -> Vec<H2HeaderField> {
    fields
        .iter()
        .map(|field| H2HeaderField {
            name: field.name.clone(),
            value: field.value.clone(),
            sensitive: field.sensitive,
        })
        .collect()
}

fn manifest_fields(fields: &[H2HeaderField]) -> Vec<Field> {
    fields
        .iter()
        .map(|field| Field {
            name: field.name.clone(),
            value: field.value.clone(),
            sensitive: field.sensitive,
        })
        .collect()
}

fn diagnostics(snapshot: H2HpackDiagnosticsSnapshot) -> HpackDiagnosticsEvidenceV3 {
    HpackDiagnosticsEvidenceV3 {
        encoded_blocks: snapshot.encoded_blocks,
        decoded_blocks: snapshot.decoded_blocks,
        indexed_fields: snapshot.indexed_fields,
        incremental_fields: snapshot.incremental_fields,
        without_indexing_fields: snapshot.without_indexing_fields,
        never_indexed_fields: snapshot.never_indexed_fields,
        huffman_strings: snapshot.huffman_strings,
        plain_strings: snapshot.plain_strings,
        table_size_updates: snapshot.table_size_updates,
        table_insertions: snapshot.table_insertions,
        table_evictions: snapshot.table_evictions,
        compression_errors: snapshot.compression_errors,
        local_limit_failures: snapshot.local_limit_failures,
        field_octets: snapshot.field_octets,
        wire_octets: snapshot.wire_octets,
    }
}

fn settings_frame() -> Vec<u8> {
    let mut bytes = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0,
        stream_id: 0,
        payload: Vec::new(),
    }
    .encode(&mut bytes);
    bytes
}

fn headers_frame(stream_id: u32, block: Vec<u8>, end_stream: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Headers,
        flags: 0x4 | u8::from(end_stream),
        stream_id,
        payload: block,
    }
    .encode(&mut bytes);
    bytes
}

fn initialized_server() -> H2Server {
    let mut server = H2Server::default();
    let input = [CLIENT_PREFACE, settings_frame().as_slice()].concat();
    let (_, consumed, _) = server.accept_event_bytes(&input).unwrap();
    assert_eq!(consumed, input.len());
    server
}

fn initialized_client() -> H2Client {
    let mut client = H2Client::default();
    let _ = client.connection_preface();
    let settings = settings_frame();
    let (_, consumed, _) = client.accept_bytes(&settings).unwrap();
    assert_eq!(consumed, settings.len());
    client
}

fn role_fields(role: PathRoleV4, source: &[H2HeaderField]) -> Vec<H2HeaderField> {
    match role {
        PathRoleV4::ServerRequest | PathRoleV4::ClientRequest | PathRoleV4::StackRequest => {
            if source.first().is_some_and(|field| field.name == b":method") {
                let mut fields = source.to_vec();
                fields.extend([
                    H2HeaderField {
                        name: b":scheme".to_vec(),
                        value: b"https".to_vec(),
                        sensitive: false,
                    },
                    H2HeaderField {
                        name: b":path".to_vec(),
                        value: b"/".to_vec(),
                        sensitive: false,
                    },
                ]);
                fields
            } else {
                let mut fields = vec![
                    H2HeaderField {
                        name: b":method".to_vec(),
                        value: b"GET".to_vec(),
                        sensitive: false,
                    },
                    H2HeaderField {
                        name: b":scheme".to_vec(),
                        value: b"https".to_vec(),
                        sensitive: false,
                    },
                    H2HeaderField {
                        name: b":path".to_vec(),
                        value: b"/".to_vec(),
                        sensitive: false,
                    },
                ];
                fields.extend_from_slice(source);
                fields
            }
        }
        PathRoleV4::ServerResponse | PathRoleV4::ClientResponse | PathRoleV4::StackResponse => {
            if source.first().is_some_and(|field| field.name == b":status") {
                source.to_vec()
            } else {
                let mut fields = vec![H2HeaderField {
                    name: b":status".to_vec(),
                    value: b"200".to_vec(),
                    sensitive: false,
                }];
                fields.extend_from_slice(source);
                fields
            }
        }
        PathRoleV4::FsmGrpcStatus | PathRoleV4::StackGrpcStatus => {
            let mut fields = source.to_vec();
            fields.push(H2HeaderField {
                name: b"grpc-status".to_vec(),
                value: b"0".to_vec(),
                sensitive: false,
            });
            fields
        }
        _ => source.to_vec(),
    }
}

fn select_source(decoded: &[H2HeaderField], source: &[H2HeaderField]) -> Vec<H2HeaderField> {
    let mut selected = Vec::with_capacity(source.len());
    let mut offset = 0;
    for expected in source {
        let index = decoded[offset..]
            .iter()
            .position(|field| field == expected)
            .expect("adapter dropped or changed a canonical occurrence");
        offset += index + 1;
        selected.push(decoded[offset - 1].clone());
    }
    selected
}

fn encode_evidence(
    role: PathRoleV4,
    source: &[H2HeaderField],
    observed: Vec<H2HeaderField>,
    decoded_binary: Applicability<Vec<u8>>,
) -> PathRoleEvidenceV4 {
    let full = role_fields(role, source);
    let mut encoder = H2HeaderBlockEncoder::new();
    let wire = encoder.try_encode_fields(&full).unwrap();
    let snapshot = encoder.diagnostics();
    let decoded = H2HeaderBlockDecoder::new()
        .try_decode_with_limit(&wire, usize::MAX)
        .unwrap();
    assert_eq!(select_source(&decoded, source), observed);
    PathRoleEvidenceV4 {
        role,
        outcome: AdapterOutcome::Preserve(manifest_fields(&observed)),
        occurrences: Applicability::Applicable(manifest_fields(&observed)),
        wire: Applicability::Applicable(wire),
        table_entries: Applicability::Applicable(
            (snapshot.table_insertions - snapshot.table_evictions) as usize,
        ),
        diagnostics: Applicability::Applicable(diagnostics(snapshot)),
        error: Applicability::NotApplicable,
        decoded_binary,
        reusable: Applicability::Applicable(true),
    }
}

fn rejection(role: PathRoleV4, error: PathErrorV4) -> PathRoleEvidenceV4 {
    PathRoleEvidenceV4 {
        role,
        outcome: AdapterOutcome::Reject,
        occurrences: Applicability::NotApplicable,
        wire: Applicability::NotApplicable,
        table_entries: Applicability::NotApplicable,
        diagnostics: Applicability::NotApplicable,
        error: Applicability::Applicable(error),
        decoded_binary: Applicability::NotApplicable,
        reusable: Applicability::Applicable(true),
    }
}

fn exercise_server_request(source: &[H2HeaderField]) -> Vec<H2HeaderField> {
    let full = role_fields(PathRoleV4::ServerRequest, source);
    let block = H2HeaderBlockEncoder::new()
        .try_encode_fields(&full)
        .unwrap();
    let mut server = initialized_server();
    let frame = headers_frame(1, block, false);
    let (event, consumed, _) = server.accept_event_bytes(&frame).unwrap();
    assert_eq!(consumed, frame.len());
    let H2ByteStreamEvent::RequestHeaders { headers, .. } = event.unwrap() else {
        panic!("expected request headers")
    };
    select_source(&headers, source)
}

fn exercise_server_response(source: &[H2HeaderField]) -> Vec<H2HeaderField> {
    let mut server = H2Server::default();
    let regular = if source.iter().any(|field| field.name.starts_with(b":")) {
        &[][..]
    } else {
        source
    };
    let commit = server
        .response_headers_frame_with_raw_headers(1, 200, regular, false)
        .unwrap();
    let bytes = server.next_outbound_block().unwrap().bytes().to_vec();
    server.acknowledge_outbound_block(commit).unwrap();
    let (frame, _) = H2Frame::decode(&bytes).unwrap();
    let decoded = H2HeaderBlockDecoder::new()
        .try_decode_with_limit(&frame.payload, usize::MAX)
        .unwrap();
    select_source(&decoded, source)
}

fn exercise_server_trailers(
    source: &[H2HeaderField],
    reject: bool,
) -> Result<Vec<H2HeaderField>, ()> {
    let request = role_fields(PathRoleV4::ServerRequest, &[]);
    let mut peer = H2HeaderBlockEncoder::new();
    let mut server = initialized_server();
    let request = headers_frame(1, peer.try_encode_fields(&request).unwrap(), false);
    server.accept_event_bytes(&request).unwrap();
    let trailers = headers_frame(1, peer.try_encode_fields(source).unwrap(), true);
    match server.accept_event_bytes(&trailers) {
        Ok((Some(H2ByteStreamEvent::Trailers { headers, .. }), _, _)) if !reject => {
            Ok(select_source(&headers, source))
        }
        Err(_) if reject => Err(()),
        other => panic!("unexpected server trailer result: {other:?}"),
    }
}

fn exercise_client_request(source: &[H2HeaderField]) -> Vec<H2HeaderField> {
    let mut client = initialized_client();
    let regular = if source.iter().any(|field| field.name.starts_with(b":")) {
        &[][..]
    } else {
        source
    };
    let (stream_id, commit) = client
        .open_stream_with_raw_headers("GET", "https", "example.test", "/", regular, true)
        .unwrap();
    let bytes = client.next_outbound_block().unwrap().bytes().to_vec();
    client.acknowledge_outbound_block(commit).unwrap();
    let (frame, _) = H2Frame::decode(&bytes).unwrap();
    assert_eq!(frame.stream_id, stream_id);
    let decoded = H2HeaderBlockDecoder::new()
        .try_decode_with_limit(&frame.payload, usize::MAX)
        .unwrap();
    select_source(&decoded, source)
}

fn exercise_client_response(source: &[H2HeaderField]) -> Vec<H2HeaderField> {
    let mut client = initialized_client();
    let (stream_id, commit) = client
        .open_stream_with_raw_headers("GET", "https", "example.test", "/", &[], true)
        .unwrap();
    client.acknowledge_outbound_block(commit).unwrap();
    let full = role_fields(PathRoleV4::ClientResponse, source);
    let block = H2HeaderBlockEncoder::new()
        .try_encode_fields(&full)
        .unwrap();
    let frame = headers_frame(stream_id, block, false);
    let (event, _, _) = client.accept_bytes(&frame).unwrap();
    let H2ByteClientEvent::ResponseHeaders { headers, .. } = event.unwrap() else {
        panic!("expected response headers")
    };
    select_source(&headers, source)
}

fn exercise_client_trailers(
    source: &[H2HeaderField],
    reject: bool,
) -> Result<Vec<H2HeaderField>, ()> {
    let mut client = initialized_client();
    let (stream_id, commit) = client
        .open_stream_with_raw_headers("GET", "https", "example.test", "/", &[], true)
        .unwrap();
    client.acknowledge_outbound_block(commit).unwrap();
    let mut peer = H2HeaderBlockEncoder::new();
    let response = role_fields(PathRoleV4::ClientResponse, &[]);
    let response = headers_frame(stream_id, peer.try_encode_fields(&response).unwrap(), false);
    client.accept_bytes(&response).unwrap();
    let trailers = headers_frame(stream_id, peer.try_encode_fields(source).unwrap(), true);
    match client.accept_bytes(&trailers) {
        Ok((Some(H2ByteClientEvent::Trailers { headers, .. }), _, _)) if !reject => {
            Ok(select_source(&headers, source))
        }
        Err(_) if reject => Err(()),
        other => panic!("unexpected client trailer result: {other:?}"),
    }
}

fn exercise_role(scenario: &PathScenarioV4, role: PathRoleV4) -> PathRoleEvidenceV4 {
    let source = h2_fields(&scenario.fields);
    let binary = if matches!(
        role,
        PathRoleV4::FsmGrpcMetadata | PathRoleV4::FsmGrpcStatus
    ) {
        scenario.decoded_binary.clone()
    } else {
        Applicability::NotApplicable
    };
    match role {
        PathRoleV4::Direct => {
            let observed = H2HeaderBlockDecoder::new()
                .try_decode_with_limit(
                    &H2HeaderBlockEncoder::new()
                        .try_encode_fields(&source)
                        .unwrap(),
                    usize::MAX,
                )
                .unwrap();
            encode_evidence(role, &source, observed, binary)
        }
        PathRoleV4::ServerRequest => {
            encode_evidence(role, &source, exercise_server_request(&source), binary)
        }
        PathRoleV4::ServerResponse => {
            encode_evidence(role, &source, exercise_server_response(&source), binary)
        }
        PathRoleV4::ServerTrailers => {
            if scenario.input == PathInput::PseudoTrailers {
                exercise_server_trailers(&source, true).unwrap_err();
                rejection(role, PathErrorV4::InvalidPseudoRole)
            } else {
                encode_evidence(
                    role,
                    &source,
                    exercise_server_trailers(&source, false).unwrap(),
                    binary,
                )
            }
        }
        PathRoleV4::ClientRequest => {
            encode_evidence(role, &source, exercise_client_request(&source), binary)
        }
        PathRoleV4::ClientResponse => {
            encode_evidence(role, &source, exercise_client_response(&source), binary)
        }
        PathRoleV4::ClientTrailers => {
            if scenario.input == PathInput::PseudoTrailers {
                exercise_client_trailers(&source, true).unwrap_err();
                rejection(role, PathErrorV4::InvalidPseudoRole)
            } else {
                encode_evidence(
                    role,
                    &source,
                    exercise_client_trailers(&source, false).unwrap(),
                    binary,
                )
            }
        }
        PathRoleV4::TextProjection => {
            let before = source.clone();
            let (projection_source, projection) = match scenario.input {
                PathInput::PseudoRequest => {
                    let fields = role_fields(PathRoleV4::ServerRequest, &source);
                    let result = project_h2_header_fields_for_role(&fields, H2HeaderRole::Request);
                    (fields, result)
                }
                PathInput::PseudoResponse => {
                    let fields = role_fields(PathRoleV4::ServerResponse, &source);
                    let result = project_h2_header_fields_for_role(&fields, H2HeaderRole::Response);
                    (fields, result)
                }
                PathInput::PseudoTrailers => {
                    let result = project_h2_header_fields_for_role(&source, H2HeaderRole::Trailers);
                    (source.clone(), result)
                }
                _ => (source.clone(), project_h2_header_fields(&source)),
            };
            match projection {
                Err(H2HeaderProjectionError::ValueNotUtf8) => {
                    assert_eq!(source, before);
                    rejection(role, PathErrorV4::ValueNotUtf8)
                }
                Err(H2HeaderProjectionError::MalformedPseudoHeaders)
                    if scenario.input == PathInput::PseudoTrailers =>
                {
                    assert_eq!(source, before);
                    rejection(role, PathErrorV4::InvalidPseudoRole)
                }
                Ok(projected) => {
                    let full = projected
                        .iter()
                        .map(|header| H2HeaderField {
                            name: header.name.as_bytes().to_vec(),
                            value: header.value.as_bytes().to_vec(),
                            sensitive: header.sensitive,
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(full.len(), projection_source.len());
                    let observed = select_source(&full, &source);
                    assert_eq!(source, before);
                    PathRoleEvidenceV4 {
                        role,
                        outcome: AdapterOutcome::Preserve(manifest_fields(&observed)),
                        occurrences: Applicability::Applicable(manifest_fields(&observed)),
                        wire: Applicability::NotApplicable,
                        table_entries: Applicability::NotApplicable,
                        diagnostics: Applicability::NotApplicable,
                        error: Applicability::NotApplicable,
                        decoded_binary: Applicability::NotApplicable,
                        reusable: Applicability::Applicable(true),
                    }
                }
                other => panic!("unexpected text projection: {other:?}"),
            }
        }
        PathRoleV4::PseudoForward => {
            let projected = project_h2_header_fields(&source).unwrap();
            assert_eq!(
                projected[0].try_as_raw_occurrence(),
                Err(H2HeaderProjectionError::PseudoHeaderNotForwardable)
            );
            rejection(role, PathErrorV4::PseudoNotForwardable)
        }
        _ => unreachable!("role belongs to another authoritative runner"),
    }
}

#[test]
fn current_manifest_phase_three_fsm_http_paths_complete_exactly_once() {
    let mut ledger = CompletionLedger::phase_three(Runner::FsmHttpAdapters);
    for case in ledger.cases().cloned().collect::<Vec<_>>() {
        let CaseInput::PathV4(scenario) = case.input.clone() else {
            unreachable!()
        };
        let source = h2_fields(&scenario.fields);
        let roles = scenario
            .roles
            .iter()
            .copied()
            .map(|role| exercise_role(&scenario, role))
            .collect();
        ledger.complete(RunnerResult {
            id: case.id,
            input: case.input,
            initial_state: case.initial_state,
            actual: ActualEvidence::PathV4(PathEvidenceV4 {
                path: scenario.path,
                input: scenario.input,
                source: manifest_fields(&source),
                source_unchanged: source == h2_fields(&scenario.fields),
                roles,
            }),
        });
    }
    ledger.finish();
}
