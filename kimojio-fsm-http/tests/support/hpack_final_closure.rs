#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use super::hpack_execution_ledger::RunnerResult;
use super::hpack_manifest::{ActualEvidence, CaseDefinition, CaseInput};

pub const COMPATIBILITY_EVIDENCE_ENV: &str = "KIMOJIO_HPACK_COMPATIBILITY_EVIDENCE_V1";

const TARGET_MANIFEST: &str = include_str!("../../../scripts/hpack-compatibility-targets.tsv");
const EXPECTED_TARGET_COUNT: usize = 67;
const EXPECTED_TUPLE_COUNT: usize = 324;
const CONFIGURATIONS: [&str; 2] = ["default", "all-features"];

#[derive(Clone, Debug, Eq, PartialEq)]
enum EvidenceStatus {
    Pass,
    NotApplicable(String),
}

#[derive(Clone, Debug)]
struct TargetRow<'a> {
    package: &'a str,
    kind: &'a str,
    name: &'a str,
    required_features: &'a str,
    documentation: bool,
    executable_test: bool,
    default_applicable: bool,
}

#[derive(Clone)]
pub struct ClosureDocuments<'a> {
    pub technical_reference: &'a str,
    pub fsm_http_guide: &'a str,
    pub stack_http_grpc_guide: &'a str,
    pub readme: &'a str,
    pub crate_readme: &'a str,
    pub inventory: &'a str,
}

impl ClosureDocuments<'static> {
    pub fn repository() -> Self {
        Self {
            technical_reference: include_str!("../../../docs/hpack-header-representation.md"),
            fsm_http_guide: include_str!("../../../docs/fsm-http-server.md"),
            stack_http_grpc_guide: include_str!("../../../docs/stack-http-grpc.md"),
            readme: include_str!("../../../README.md"),
            crate_readme: include_str!("../../README.md"),
            inventory: include_str!("../../TODO.md"),
        }
    }
}

pub struct FinalClosureRunner<'a> {
    observed: BTreeMap<String, EvidenceStatus>,
    documents: ClosureDocuments<'a>,
}

impl FinalClosureRunner<'static> {
    pub fn from_environment() -> Result<Self, String> {
        let evidence = std::env::var(COMPATIBILITY_EVIDENCE_ENV)
            .map_err(|_| format!("{COMPATIBILITY_EVIDENCE_ENV} is not set"))?;
        Self::new(&evidence, ClosureDocuments::repository())
    }
}

impl<'a> FinalClosureRunner<'a> {
    pub fn new(evidence: &str, documents: ClosureDocuments<'a>) -> Result<Self, String> {
        Ok(Self {
            observed: parse_observed_evidence(evidence)?,
            documents,
        })
    }

    pub fn result_for(&self, case: &CaseDefinition) -> Result<RunnerResult, String> {
        let CaseInput::Closure(criterion) = &case.input else {
            return Err(format!("{} is not a closure case", case.id));
        };
        self.verify_criterion(*criterion)?;
        Ok(RunnerResult {
            id: case.id.clone(),
            input: case.input.clone(),
            initial_state: case.initial_state,
            actual: ActualEvidence::Closure {
                criterion: *criterion,
            },
        })
    }

    fn verify_criterion(&self, criterion: u8) -> Result<(), String> {
        match criterion {
            1 => self.verify_constituent_runners(&[
                ("kimojio-fsm-http", "hpack_api"),
                ("kimojio-fsm-http", "hpack_manifest"),
                ("kimojio-fsm-http", "hpack_resource_bounds"),
                ("kimojio-fsm-http", "hpack_vectors"),
            ]),
            2 => self.verify_constituent_runners(&[
                ("kimojio-fsm-http", "hpack_adapters"),
                ("kimojio-fsm-grpc", "hpack_metadata"),
                ("kimojio-stack-http", "hpack_adapters"),
                ("kimojio-stack-grpc", "hpack_metadata"),
            ]),
            3 => self.verify_constituent_runners(&[("kimojio-fsm-http", "hpack_connection")]),
            4 => self.verify_constituent_runners(&[("kimojio-fsm-http", "hpack_diagnostics")]),
            5 => self.verify_complete_compatibility_matrix(),
            6 => {
                for prerequisite in 1..=5 {
                    self.verify_criterion(prerequisite)?;
                }
                self.verify_documents_and_inventory()
            }
            _ => Err(format!("unknown closure criterion SC-{criterion:03}")),
        }
    }

    fn verify_constituent_runners(&self, runners: &[(&str, &str)]) -> Result<(), String> {
        for (package, target) in runners {
            for configuration in CONFIGURATIONS {
                let tuple = format!("{package}|{configuration}|test:{target}|test");
                match self.observed.get(&tuple) {
                    Some(EvidenceStatus::Pass) => {}
                    Some(status) => {
                        return Err(format!(
                            "constituent runner {tuple} did not pass: {status:?}"
                        ));
                    }
                    None => return Err(format!("constituent runner result is missing: {tuple}")),
                }
            }
        }
        Ok(())
    }

    fn verify_complete_compatibility_matrix(&self) -> Result<(), String> {
        let expected = expected_compatibility_matrix()?;
        if self.observed != expected {
            let missing = expected
                .keys()
                .filter(|tuple| !self.observed.contains_key(*tuple))
                .cloned()
                .collect::<Vec<_>>();
            let unexpected = self
                .observed
                .keys()
                .filter(|tuple| !expected.contains_key(*tuple))
                .cloned()
                .collect::<Vec<_>>();
            let mismatched = expected
                .iter()
                .filter_map(|(tuple, status)| {
                    self.observed
                        .get(tuple)
                        .filter(|actual| *actual != status)
                        .map(|actual| format!("{tuple}: expected {status:?}, observed {actual:?}"))
                })
                .collect::<Vec<_>>();
            return Err(format!(
                "compatibility evidence mismatch; missing={missing:?}, \
                 unexpected={unexpected:?}, mismatched={mismatched:?}"
            ));
        }
        Ok(())
    }

    fn verify_documents_and_inventory(&self) -> Result<(), String> {
        require_all(
            "technical reference",
            self.documents.technical_reference,
            &[
                "## Codec and representation",
                "## Connection ownership and wire order",
                "### Checked outbound handoff",
                "if block.commit() != commit",
                "## Byte and text APIs",
                "`try_into_text` consumes",
                "## Limits and errors",
                "## Diagnostics",
                "## HTTP and gRPC adapters",
                "## Acceptance and compatibility",
                "scripts/hpack-compatibility-targets.tsv",
                "324 target/action tuples",
                "## SC-006 documentation checklist",
                "[FSM HTTP Server Guide]",
                "[Stackful HTTP and gRPC Guide]",
                "[`kimojio-fsm-http/TODO.md`]",
                "[README technical-reference link]",
                "[README FSM-guide link]",
                "[README stack-guide link]",
            ],
        )?;
        require_all(
            "FSM HTTP guide",
            self.documents.fsm_http_guide,
            &[
                "# FSM HTTP Server",
                "### Outbound header handoff migration",
                "block.commit() == commit",
                "checked outbound handoff",
                "### HPACK headers, limits, and diagnostics",
                "[HPACK and Header Representation Reference]",
            ],
        )?;
        require_all(
            "stack HTTP/gRPC guide",
            self.documents.stack_http_grpc_guide,
            &[
                "# Stackful HTTP and gRPC",
                "### HPACK ownership, limits, and diagnostics",
                "Canonical `h2::HeaderField`",
                "content-free directional",
                "[HPACK and Header Representation Reference]",
                "target/action",
            ],
        )?;
        require_all(
            "README",
            self.documents.readme,
            &[
                "[Stackful HTTP and gRPC Guide][stack-http-grpc-guide]",
                "[HPACK and Header Representation Reference][hpack-header-representation]",
                "[FSM HTTP Server Guide][fsm-http-server-guide]",
                "[stack-http-grpc-guide]: https://github.com/Azure/kimojio-rs/blob/main/docs/stack-http-grpc.md",
                "[hpack-header-representation]: https://github.com/Azure/kimojio-rs/blob/main/docs/hpack-header-representation.md",
                "[fsm-http-server-guide]: https://github.com/Azure/kimojio-rs/blob/main/docs/fsm-http-server.md",
            ],
        )?;
        require_all(
            "crate README",
            self.documents.crate_readme,
            &[
                "# Kimojio FSM HTTP",
                "## Responsibilities",
                "## Runtime-independent progress model",
                "## HTTP/1.1 entry points",
                "## HTTP/2 entry points",
                "## HPACK ownership and outbound handoff",
                "## Runtime adapters",
                "## Limitations",
                "[`TODO.md`](TODO.md)",
            ],
        )?;
        require_all(
            "inventory orientation pointer",
            self.documents.inventory,
            &[
                "[HPACK and Header Representation Reference](../docs/hpack-header-representation.md)",
            ],
        )?;

        for completed in [
            "DRIVER-001",
            "DRIVER-004",
            "DRIVER-007",
            "SEC-007",
            "TEST-010",
            "BODY-001",
            "CONN-001",
            "DRIVER-002",
            "DRIVER-003",
            "HPACK-005",
            "HPACK-006",
            "INTEG-001",
            "MSG-001",
            "MSG-002",
            "SEC-006",
            "TEST-003",
        ] {
            if self.documents.inventory.contains(completed) {
                return Err(format!("completed inventory row remains: {completed}"));
            }
        }
        let actual_ids = inventory_ids(self.documents.inventory);
        let expected_ids = [
            "CTRL-004 / SEC-004",
            "CTRL-005 / CTRL-006 / SEC-001 / SEC-003",
            "DRIVER-005",
            "DRIVER-006",
            "OPT-001 / OPT-004",
            "OPT-003",
            "OPT-005",
            "SEC-005",
            "SEC-008",
            "SEC-009",
            "STREAM-007",
            "STREAM-008",
            "STREAM-009",
            "TEST-002",
            "TEST-004",
            "TEST-004B",
            "TEST-006",
            "TEST-007",
            "TEST-009",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
        if actual_ids != expected_ids {
            return Err(format!(
                "inventory ID set drift; expected={expected_ids:?}, actual={actual_ids:?}"
            ));
        }
        Ok(())
    }
}

fn target_rows() -> Result<Vec<TargetRow<'static>>, String> {
    let mut rows = Vec::new();
    let mut keys = BTreeSet::new();
    for line in TARGET_MANIFEST
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let columns = line.split('|').collect::<Vec<_>>();
        if columns.len() != 7 {
            return Err(format!("invalid target denominator row: {line}"));
        }
        let documentation = parse_flag(columns[4], line)?;
        let executable_test = parse_flag(columns[5], line)?;
        let default_applicable = parse_flag(columns[6], line)?;
        if !matches!(columns[1], "lib" | "bin" | "example" | "test") {
            return Err(format!("unsupported target kind: {line}"));
        }
        if !default_applicable && columns[3] == "-" {
            return Err(format!(
                "non-applicable target lacks required features: {line}"
            ));
        }
        let key = (columns[0], columns[1], columns[2]);
        if !keys.insert(key) {
            return Err(format!("duplicate target denominator key: {key:?}"));
        }
        rows.push(TargetRow {
            package: columns[0],
            kind: columns[1],
            name: columns[2],
            required_features: columns[3],
            documentation,
            executable_test,
            default_applicable,
        });
    }
    if rows.len() != EXPECTED_TARGET_COUNT {
        return Err(format!(
            "target denominator count mismatch: expected {EXPECTED_TARGET_COUNT}, found {}",
            rows.len()
        ));
    }
    Ok(rows)
}

fn parse_flag(value: &str, row: &str) -> Result<bool, String> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(format!("invalid target flag in row: {row}")),
    }
}

fn expected_compatibility_matrix() -> Result<BTreeMap<String, EvidenceStatus>, String> {
    let mut expected = BTreeMap::new();
    for row in target_rows()? {
        for configuration in CONFIGURATIONS {
            let status = if configuration == "default" && !row.default_applicable {
                EvidenceStatus::NotApplicable(row.required_features.to_owned())
            } else {
                EvidenceStatus::Pass
            };
            insert_expected(
                &mut expected,
                format!(
                    "{}|{configuration}|{}:{}|check",
                    row.package, row.kind, row.name
                ),
                status.clone(),
            )?;
            if row.documentation {
                insert_expected(
                    &mut expected,
                    format!(
                        "{}|{configuration}|{}:{}|rustdoc",
                        row.package, row.kind, row.name
                    ),
                    status.clone(),
                )?;
            }
            if row.executable_test {
                insert_expected(
                    &mut expected,
                    format!(
                        "{}|{configuration}|{}:{}|test",
                        row.package, row.kind, row.name
                    ),
                    status.clone(),
                )?;
            }
        }
    }
    if expected.len() != EXPECTED_TUPLE_COUNT {
        return Err(format!(
            "tuple denominator count mismatch: expected {EXPECTED_TUPLE_COUNT}, found {}",
            expected.len()
        ));
    }
    Ok(expected)
}

fn insert_expected(
    expected: &mut BTreeMap<String, EvidenceStatus>,
    tuple: String,
    status: EvidenceStatus,
) -> Result<(), String> {
    if expected.insert(tuple.clone(), status).is_some() {
        return Err(format!("duplicate expected compatibility tuple: {tuple}"));
    }
    Ok(())
}

fn parse_observed_evidence(evidence: &str) -> Result<BTreeMap<String, EvidenceStatus>, String> {
    if evidence.trim().is_empty() {
        return Err("compatibility evidence is empty".to_owned());
    }
    let mut observed = BTreeMap::new();
    for line in evidence.lines() {
        let (tuple, status) = if let Some(tuple) = line.strip_prefix("PASS ") {
            (tuple, EvidenceStatus::Pass)
        } else if let Some(rest) = line.strip_prefix("NOT_APPLICABLE ") {
            let (tuple, required_features) = rest
                .split_once(" required-features=")
                .ok_or_else(|| format!("invalid non-applicable evidence: {line}"))?;
            (
                tuple,
                EvidenceStatus::NotApplicable(required_features.to_owned()),
            )
        } else {
            return Err(format!("invalid compatibility evidence: {line}"));
        };
        if tuple.is_empty() {
            return Err(format!("empty compatibility tuple: {line}"));
        }
        if observed.insert(tuple.to_owned(), status).is_some() {
            return Err(format!("duplicate compatibility evidence: {tuple}"));
        }
    }
    Ok(observed)
}

fn require_all(surface: &str, content: &str, required: &[&str]) -> Result<(), String> {
    for needle in required {
        if !content.contains(needle) {
            return Err(format!("{surface} is missing required content: {needle}"));
        }
    }
    Ok(())
}

fn inventory_ids(inventory: &str) -> BTreeSet<String> {
    inventory
        .lines()
        .filter_map(|line| {
            let line = line.strip_prefix("| ")?;
            let id = line.split('|').next()?.trim();
            (!matches!(id, "ID" | "---")).then(|| id.to_owned())
        })
        .collect()
}

pub fn complete_compatibility_evidence_for_negative_tests() -> String {
    expected_compatibility_matrix()
        .expect("fixed compatibility denominator")
        .into_iter()
        .map(|(tuple, status)| match status {
            EvidenceStatus::Pass => format!("PASS {tuple}"),
            EvidenceStatus::NotApplicable(required_features) => {
                format!("NOT_APPLICABLE {tuple} required-features={required_features}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
