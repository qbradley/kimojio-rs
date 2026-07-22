#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use super::hpack_manifest::{
    AcceptanceClass, ActualEvidence, CaseDefinition, CaseInput, InitialState, Observable,
    manifest_v1, manifest_v2, manifest_v3, manifest_v4, required_observables,
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Phase {
    One,
    Two,
    Three,
    Four,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Runner {
    HpackVectors,
    HpackResourceBounds,
    HpackApi,
    HpackConnection,
    HpackDiagnostics,
    FsmHttpAdapters,
    StackHttpAdapters,
    FsmGrpcMetadata,
    StackGrpcMetadata,
    FinalCompatibility,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionState {
    Active,
    Planned,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionLedgerRow {
    pub id: String,
    pub phase: Phase,
    pub runner: Runner,
    pub state: ExecutionState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerResult {
    pub id: String,
    pub input: CaseInput,
    pub initial_state: InitialState,
    pub actual: ActualEvidence,
}

fn execution_owner(case: &CaseDefinition) -> (Phase, Runner) {
    match case.class {
        AcceptanceClass::Int
        | AcceptanceClass::Huff
        | AcceptanceClass::Rep
        | AcceptanceClass::Table
        | AcceptanceClass::Corpus => (Phase::One, Runner::HpackVectors),
        AcceptanceClass::Resource => (Phase::One, Runner::HpackResourceBounds),
        AcceptanceClass::Api => (Phase::One, Runner::HpackApi),
        AcceptanceClass::Limit if case.id.contains("ALLOC-") => (Phase::One, Runner::HpackApi),
        AcceptanceClass::Limit => (Phase::One, Runner::HpackVectors),
        AcceptanceClass::Connection if case.id.contains("DIAGNOSTICS") => {
            (Phase::Two, Runner::HpackDiagnostics)
        }
        AcceptanceClass::Connection => (Phase::Two, Runner::HpackConnection),
        AcceptanceClass::Path if case.id.starts_with("PATH-05") => {
            (Phase::Three, Runner::StackHttpAdapters)
        }
        AcceptanceClass::Path if case.id.starts_with("PATH-06") => {
            (Phase::Three, Runner::FsmGrpcMetadata)
        }
        AcceptanceClass::Path if case.id.starts_with("PATH-07") => {
            (Phase::Three, Runner::StackGrpcMetadata)
        }
        AcceptanceClass::Path => (Phase::Three, Runner::FsmHttpAdapters),
        AcceptanceClass::Closure => (Phase::Four, Runner::FinalCompatibility),
    }
}

fn execution_ledger(manifest: Vec<CaseDefinition>) -> Vec<ExecutionLedgerRow> {
    manifest
        .into_iter()
        .map(|case| {
            let (phase, runner) = execution_owner(&case);
            ExecutionLedgerRow {
                id: case.id,
                phase,
                runner,
                state: ExecutionState::Active,
            }
        })
        .collect()
}

pub fn execution_ledger_v1() -> Vec<ExecutionLedgerRow> {
    execution_ledger(manifest_v1())
}

pub fn execution_ledger_v2() -> Vec<ExecutionLedgerRow> {
    execution_ledger(manifest_v2())
}

pub fn execution_ledger_v3() -> Vec<ExecutionLedgerRow> {
    execution_ledger(manifest_v3())
}

pub fn execution_ledger_v4() -> Vec<ExecutionLedgerRow> {
    execution_ledger(manifest_v4())
}

pub struct CompletionLedger {
    runner: Runner,
    expected: BTreeMap<String, CaseDefinition>,
    completed: BTreeMap<String, BTreeSet<Observable>>,
}

impl CompletionLedger {
    pub fn phase_one(runner: Runner) -> Self {
        Self::for_phase(Phase::One, runner)
    }

    pub fn phase_two(runner: Runner) -> Self {
        Self::for_phase(Phase::Two, runner)
    }

    pub fn phase_three(runner: Runner) -> Self {
        Self::for_phase(Phase::Three, runner)
    }

    pub fn phase_four(runner: Runner) -> Self {
        Self::for_phase(Phase::Four, runner)
    }

    fn for_phase(phase: Phase, runner: Runner) -> Self {
        let owned = execution_ledger_v4()
            .into_iter()
            .filter(|row| row.phase == phase && row.runner == runner)
            .map(|row| row.id)
            .collect::<BTreeSet<_>>();
        let expected = manifest_v4()
            .into_iter()
            .filter(|case| owned.contains(&case.id))
            .map(|case| (case.id.clone(), case))
            .collect();
        Self {
            runner,
            expected,
            completed: BTreeMap::new(),
        }
    }

    pub fn cases(&self) -> impl Iterator<Item = &CaseDefinition> {
        self.expected.values()
    }

    pub fn complete(&mut self, result: RunnerResult) {
        let expected = self
            .expected
            .get(&result.id)
            .unwrap_or_else(|| panic!("case {} is not active for {:?}", result.id, self.runner));
        assert_eq!(
            result.input, expected.input,
            "runner input drift for {}",
            result.id
        );
        assert_eq!(
            result.initial_state, expected.initial_state,
            "runner initial-state drift for {}",
            result.id
        );
        let mut asserted = expected
            .expected
            .assert_matches(&result.actual)
            .into_iter()
            .collect::<BTreeSet<_>>();
        asserted.extend([Observable::Input, Observable::InitialState]);
        assert!(
            self.completed.insert(result.id.clone(), asserted).is_none(),
            "duplicate completion {}",
            result.id
        );
    }

    pub fn finish(self) {
        let expected_ids = self.expected.keys().cloned().collect::<BTreeSet<_>>();
        let completed_ids = self.completed.keys().cloned().collect::<BTreeSet<_>>();
        assert_eq!(
            completed_ids, expected_ids,
            "missing or unexpected completion for {:?}",
            self.runner
        );
        for (id, case) in self.expected {
            let asserted = &self.completed[&id];
            assert!(asserted.contains(&Observable::Input));
            assert!(asserted.contains(&Observable::InitialState));
            for requirement in case.requirement_links {
                for observable in required_observables(requirement) {
                    assert!(
                        asserted.contains(observable),
                        "{id} links {requirement} but did not assert {observable:?}"
                    );
                }
            }
        }
    }
}
